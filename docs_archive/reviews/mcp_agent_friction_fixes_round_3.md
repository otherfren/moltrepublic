# Fix plan, round 3: the cut that partitions, and the machine the agent operates

Status: **EXECUTED 2026-09-06 - every part built the same night; the leftovers are in "What stays open" at the end.** The user
approved the proposals of the round-3 discussion as written
(`docs_archive/reviews/mcp_agent_friction_2026-09-06_r3.md` §6, items 1-7)
and decided item 8 by ADR-0007
(`docs_archive/adr/0007-agent-operates-the-machine.md`). Order of work
is the order of damage: A and F first (the partition and the machine
boundary, both blocking the next agent round), then B, C, D, E, G. Each
part is red-first and lands green on master; this document moves to
`docs_archive/reviews/` with the change that closes its last part.

Evidence for Part A: `docs_archive/reviews/agent_wiki_round_3/diag-right/`
(Right's `wiki_changes`, `read_chain`, `list_proposals` and its node log,
dumped BEFORE the restart) and the three friction logs beside it.

## Part A - the cut (D9, D3, R1, R20, R3, R11, R24) - BUILT 2026-09-06 (0cbd6bd5; stale-signer threshold 82a54968)

A1 **Root cause of the fold divergence, pinned red first.** Facts: at
equal height 94 Right's fold counted 26 applied patches, Left's and
Center's 24; all 197 pages were byte-identical across nodes; Right
co-signed the cut at 97 (`receive_checkpoint_proposal` recomputed the
state hash and it matched) and then refused the sealed block in the
receive path with `summarize`'s base mismatch (`have` = its held base
d947…, `want` = 28d28805…), while Left, a non-signer, applied it; after
the cut Left's fold reported `base = 2334dce1…`. Three code sites
compute "the memory group over the held base": `cmd_propose_checkpoint`
(checkpoint.rs:114), `own_cut_state` (governance.rs:874, the co-sign
and the apply) and the verifier (`verify.rs:682`, the receive). Work:
(1) reproduce with a three-node keystone over `MockRelay`
(`crates/molt-engine/tests/checkpoint_under_load.rs`): a republic with
a first cut, then 30+ wiki patches with at least one VOID patch (a patch
that fails to apply on one node's tree, e.g. a `replace` racing a rename)
and open proposals, then an auto-cut; assert every node applies the cut,
holds the same base commitment, reports the same `wiki_rev`, and
`diverged` stays empty; (2) find the two extra revisions in Right's
`wiki_changes` dump (revs 25-26 vs the others' history) - the suspects,
in order: a patch VOID on two nodes and applied on the third
(`fold_wiki_step` counts only successful applies), the receive path
folding a different group than the co-sign path (the sealed block's
summarized group against the receiver's OLD held base), and the
adoption order of the new base (`governance.rs:673` adopts on apply;
the verifier runs before it); (3) make all three sites ONE function
that takes the chain and the held base and returns (state, tree,
commitment), and make the co-sign REFUSE unless the recomputed
commitment equals the proposer's AND the receiver's apply uses the
same function. Deliverable: the keystone green, and a sentence in
`persistent_chain.md` / `knowledge_base_scale.md` §4.9 naming the
invariant "a full holder folds from its blob, a suffix holder from its
fetched base, and both reach the proposer's commitment".

A2 **A cut needs every seat.** `cmd_propose_checkpoint` announces the
commitment; the seal happens only when EVERY roster member has
co-signed (n-of-n, like the genesis), not at m. A member that does not
answer leaves the cut pending; `maybe_auto_checkpoint` re-proposes at
the next head as today. Rationale (user, 2026-09-06): a cut has no
hurry, and a cut a seat cannot reproduce must never seal. Test: a
two-of-three cut with one silent seat does not seal; it seals when the
third answers.

A3 **`wiki_rev` never resets.** The cut records the fold revision at the
cut in the memory group's base entry (`rev_at_cut`, additive); a fold
that starts from a held base starts counting at that number. `wiki_changes
since_rev` below the cut keeps answering `truncated: true`. The sync
line of the agents stays comparable across a cut. Test: rev before the
cut N, after the cut N (no patches) and N+1 after the next patch, on
every node.

A4 **`superseded` split in two.** `read_proposal` and `list_proposals`
carry `superseded: "rebase" | "conflict" | null`: `rebase` = the base
moved under it but the patch still applies (the cut's wave: R1) - the
proposal stays votable, keeps its votes and the proposer's signature,
and is re-anchored at the new base by the engine; `conflict` = a
committed change touched its paths and the patch no longer applies -
dead, and `list_proposals` says so. The `⚖` line names which. Tests: the
five-proposal wave of round 3 replayed (open proposals + a cut) keeps
every vote; a real conflict reads `conflict`; a re-proposal after a
conflict needs no `allow_warnings` for the caller's own dead proposal
(R19's second half).

A5 **Silence and lag are reported.** `status` gains `chain_lag {
peers_ahead: u64, silent: [{member, secs}] }` from two facts the node
already has: the highest height any peer announced (blocks it received
or refused, proposals it saw sealed) and `last_seen`; `read_chain` gains
`stale_signers: [{member, last_signed_height}]`. The tool texts say what
`diverged` measures (contradiction) and what these measure (silence and
distance). Test: a node cut off at 96 while peers seal 98+ reads
`peers_ahead >= 2` within one delivery tick; a seat that stops co-signing
appears in `stale_signers` after N blocks.

A6 **A vote at the wrong height is refused, not counted.** `approve` /
`decline` refuse with `MoltError::ChainLagging` when `chain_lag.peers_ahead
> 0`; the vote reply carries `height`; a receiver that drops a foreign
vote as "implausible height" sends the sender a `net_vote_refused`
control frame (INTERNAL) that the sender surfaces as a notice. Test: the
round-3 situation (Right approving at 97 while the others are at 100+)
answers a refusal on Right.

## Part B - restart durability (R12, R17, R2) - BUILT 2026-09-06 (0188fa8d)

What the build found: foreign governance events were never written to the
own log (a reopen rebuilt them from the catch-up re-serve, re-authored as
own), `pending_sigs` was never rebuilt at open, and the reopen order on a
folded holder skipped the supersede walk. All three fixed; the peer's
events now ride the own log under the peer's name and are never
re-broadcast. Open: a card that reaches a node ONLY through a catch-up
re-serve is still attributed to the serving peer.

B1 The proposal store persists `by`, every vote and the proposer's own
signature across a reopen (today: reload re-labels foreign proposals as
own and forgets the votes). Test: reopen keeps `by`, `mine`, `approvals`
and `votes` of an open foreign proposal and of an own one.

B2 A pure `create` patch onto a path that the base already carries is
`superseded: "conflict"` at reload and at receive (R2's dead proposals).

B3 `withdraw` takes `note` (posted into the patch channel before the
withdrawal) and answers the proposal record like approve/decline.

## Part C - log noise (D10, D11) - BUILT 2026-09-06 (0188fa8d + 60d4bf13)

D11 was a harness artefact: `RUST_LOG=info` replaced the default filter's
`openmls=off`; `molt-app` now appends it unless the value names openmls.

C1 The decision line's deterministic id is RECOGNIZED by the ingest: the
guard recomputes `decision_summary_id(proposal)` for a cross-author
duplicate and logs it at debug when it matches; only a foreign id
collision stays WARN. Test: two nodes tipping the same proposal produce
no WARN.

C2 openmls's `SecretReuseError` / "generation out of bounds" for a
resent frame goes through the engine's stale-frame filter (debug), like
the 2026-09-03 fix did for the other replay class; a genuine decrypt
failure stays one WARN line.

## Part D - the file plane (R21, R18, R5, R10, R4, R23, R22) - BUILT 2026-09-06 (48090f6b)

Open: a bare `touch` on a shared file reads `changed` (the stamp is the
cheap witness; the remedy is a new share); `gone` means "nowhere" and
sits below `mirrored` in the precedence.

D1 **A share is immutable.** `share_file` records (size, mtime) beside
the path; `uploads_view` / `local_copy_of` re-check both on read and mark
a changed file `available: false` with `availability: "changed"`; the
sharer's serve path already refuses on hash. The tool text says a share
must not be replaced on disk - a new version is a new file. Test: the
round-3 hazard (overwrite in the exchange folder) reads `changed` on the
next `read_uploads`.

D2 **`download_file` serves from the local mirror first.** When
`local_copy_of` is `Mirrored`, the download assembles the pieces into
the exchange folder (the `ReadUploadBytes` source order, re-hashed) and
answers `done` without a network fetch; `read_uploads` says where the
bytes are (`local: own|downloaded|mirrored|partial|none`, additive).
Test: a mirrored file downloads with the relay stopped.

D3 **A reference must name a known, persistent file** (user, Q2
revisited): `wiki_edit` refuses an `upload:` reference that resolves to
nothing, to more than one file, OR only to a temporary share - three
warning texts, all refusing without `allow_warnings`; the pane's
`temporary` card stays for pages that were written before the vote
moved. `wiki_health` and `wiki_get` answer the `head` and base
commitment they measured on (R10). Test: the round-3 experiment
"reference before the vote" is refused.

D4 **Tool text and fields:** `availability` explained where it appears
(and it reads `mirrored` when this seat or a peer holds the whole
series); `mirror_held/of` and `mirrors` say pieces vs members; `unpersist`'s
`at` documented; `propose` names the `files` ops and that `files` is a
core surface; `read_chain` blocks carry `ts` from the local log
(additive display field); `wiki_list {prefix}` says "folder"; `upload:`
warnings tell a malformed hex from an unknown one.

D5 **Reverse lookup:** the search index tokenizes `upload:` destinations
as a `files` field, so `wiki_search "<hex>"` finds the pages; `wiki_get.files`
stays.

## Part E - wiki hygiene (R19, R15, R6, R25, R7) - BUILT 2026-09-06 (880715f7)

The rename repair reports (`repaired`) and does not warn; `repair_links:
false` refuses. Warning codes: header, open_path, rename_link,
name_collision, foreign_rewrite, file_ref.

E1 `wiki_edit`'s title/alias and open-path warnings are computed against
the WORKING COPY after the whole edit list (not the base); a warning names
its code, and `allow_warnings` may be a list of codes to acknowledge (a
bool still means all). Test: Left's alias move (set_props then create in
one call) proposes without a warning; a real collision in the same call
still warns.

E2 `rename` rewrites the markdown links that name the OLD PATH in every
page of the base that carries one, in the same patch, and its warning names the
in-links it rewrote; a caller may pass `repair_links: false` to get the
old behaviour with a refusal instead of silence.

E3 `wiki_health` gains `props_by_type` (per `type`, the key histogram)
and `direction_outliers` (a predicate whose subject-type/object-type
pair is rare under the wiki's own distribution), both heuristic and
labelled so. Test: the 13 reversed `authored_by` edges of round 2 show up.

E4 `wiki_health` / `wiki_neighbors` answer `index_building` as a reply
field, not a hard error, while the index rebuilds.

## Part F - the machine boundary (ADR-0007; D1, D5 of the protocol) - BUILT 2026-09-06 (81a0c44c)

A relative destination with separators is REFUSED (it would resolve
against the daemon's cwd); `set_mirror_dir` still stores its path raw
(open). The walk's full run is part of the exhaustive run.

F1 The recovery phrase: drop `skip_serializing` from `WorkspaceInfo.seed`,
`create.seed`, `join.seed` (the wire shape gains the fields back, older
readers ignore them); `read_session` serves them in Seat scope only (the
Read scope's session view strips them - test
`no_recovery_phrase_ever_serializes` becomes `no_recovery_phrase_reaches_the_read_scope`);
`confirm_seed_backup`'s text says where the phrase comes from; the MCP
export carries the seed like the GUI's.

F2 Host posture: `SetNodePosture` becomes the tool `set_node_posture`
(Seat); `patch_settings` accepts the posture keys; `save_settings`
carries them; the co-equality test's INTERNAL list shrinks by it.

F3 Any-path: `share_file` takes `path` OR `name`; `download_file` takes
`dest` as a path; `export_workspace` and `wiki_export` take a path;
`set_mirror_dir` is a tool. The exchange-folder forms stay.

F4 Clearnet consent: `relay_confirm {accept_clearnet: true}` and
`relay_clearnet_session {unlock: true}` work over MCP.

F5 `docs_archive/security/mcp-security.md`: the host-boundary section is
rewritten as "The machine boundary (ADR-0007)" listing what stays
INTERNAL and why; `scripts/gui_walk.py` runs end to end again (its relay
step may use `relay_confirm` again or keep the config form); phases 3-5
re-verified; the known-debt entry closes.

## Part G - build and docs (D2, D6, D7, D8) - BUILT 2026-09-06 (G2/G3 60d4bf13, 2e50f046; G1 with the archive commit)

G1 The two GUI tests red in the normal profile: run them at `df798858`
once (one window build); then fix either the element lookup in the
testing backend usage or the tests, so the ordinary suite is true in
both flavours; the normal-profile GUI shard joins the exhaustive run.
BUILT 2026-09-06: not a lookup bug but the suite's own convention - the
compiled window carries no Slint element info, so EVERY `ElementHandle`
test is live-preview-only (73 of them; a diagnostic in the normal
profile read 0 descendants); these two had lost their gate (b37a3dbe
slid it onto the neighbouring test). Gated, the rule is in CLAUDE.md,
the way to test the generated window is in `known_debt.md`.

G2 CLAUDE.md: `cargo build -p molt-app --features ui-testing` is a second
window build; `cargo clippy` on molt-ui in the normal profile check-builds
the window module - run it once per change-set with the same `-j 1` care,
never beside a live run.

G3 `scripts/check-doc-refs.py` skips files `.gitignore` ignores.

## Agent split and ownership

Five worktree agents, one orchestrator review per branch, merge order
A → F → B/C → D → E → G:

* **Agent A** (chain): `crates/molt-engine/src/chain/*`, `crates/molt-engine/tests/checkpoint_under_load.rs`, the
  `Reply::Status`/`Reply::Chain` additive fields in molt-core, the
  `status`/`read_chain`/`approve`/`decline` texts in molt-mcp. Owns the
  D9 investigation.
* **Agent F** (machine boundary): molt-core view structs (`seed`), molt-engine
  settings/`SetNodePosture`/paths, molt-mcp tools + tests, `mcp-security.md`,
  `scripts/gui_walk.py`, known_debt.
* **Agent BC** (store + noise): proposal store persistence (molt-engine
  `proposals.rs` store + molt-storage), `withdraw`, the ingest guard,
  the MLS log filter.
* **Agent D** (files): `files_state.rs`, `transfer.rs`, `upload_refs.rs`,
  `net/files.rs`, the wiki_edit file-reference check in `proposals.rs`
  (one function), the search index field, molt-mcp file tool texts.
* **Agent E** (wiki): `proposals.rs` wiki handlers (warnings against the
  working copy, rename repair), `wiki_index` health groups, molt-mcp wiki
  tool texts.
* **G** stays with the orchestrator (a window build, CLAUDE.md, the
  checker).

Rules as in the previous rounds (`AGENT_RULES.md`): own worktree, own
branch, no window build, live-preview flavour only for molt-ui, TDD,
clippy 0, compact user-facing text, report with numbers.

## What stays open (2026-09-06, after the archive)

- `wiki_edit`: a re-proposal after a conflict still needs `allow_warnings`
  for the caller's OWN dead proposal (R19's second half, Part E).
- A card that reaches a node ONLY through a catch-up re-serve is attributed
  to the serving peer, not to its author (Part B).
- A bare `touch` on a shared file reads `changed` (Part D).
- `set_mirror_dir` stores its path raw; every other path tool resolves
  through `resolve_host_path` (Part F).
- `stale_signers` on a holder without history below its anchor (a fresh
  recovery, a pruned cut) reads a seat's last height as 0 until that seat
  signs again - a false "silent" for the first two blocks (Part A).
- `rev_at_cut` rides in the folded base entry and changes the fold
  commitment: a mixed fleet cannot co-sign a cut until every seat runs a
  build that folds the same way (Part A; a cut needs n-of-n, so the older
  seat holds it back rather than forking).
- The compiled window is invisible to the testing backend's element
  queries (`known_debt.md`, "The compiled window is invisible").
