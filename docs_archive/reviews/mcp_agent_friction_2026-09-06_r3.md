# MCP friction log, round 3: the same republic, completed, with pictures (2026-09-06, 16:33-17:47)

Status: CLOSED - observation log of the third three-agent wiki round, run
the same day the wiki file-reference feature landed
(`docs_archive/ui/wiki_files_and_images.md`). Nothing here is fixed yet:
the user decided that every defect is LOGGED and discussed first. The
discussion agenda is §6. Rounds 1 and 2: `mcp_agent_friction_2026-09-05.md`,
`mcp_agent_friction_2026-09-06.md`, their fixes `mcp_agent_friction_fixes.md`
and `mcp_agent_friction_fixes_round_2.md`. Primary evidence of this round:
`agent_wiki_round_3/` (the briefing, the three seats' friction logs, the
per-minute head timeline).

## 1. Setup

The SAME live 2-of-3 republic as round 2 ("Second Wiki Test", chain-governed,
`memory` on, one onion relay over the local Tor), continued from where round
2 left it (head 59 after a checkpoint, 125 pages, `wiki_health` 0/0/0). The
three nodes ran HEADLESS on the master build `3d05f767` (the file-reference
round merged, every crate's tests green) from scratch copies of the user's
configs - same workspace dirs, same ports and tokens, `headless = true`, and
one private exchange folder per node instead of the shared `~/Downloads`.
Seats and Opus agents as in round 2: A = Mr. Left (4040, Nostr + Bluesky;
data-obsessed, talkative, impatient), B = Mr. Center (4041, Matrix + XMPP +
ActivityPub, owner of the ontology; structure-loving, enthusiastic,
cautious), C = Mr. Right (4042, crypto layer, people/orgs, overviews;
skeptical, pedantic, terse). The briefing v3 (`agent_wiki_round_3/briefing_v3.md`)
kept round 2's cooperation rules and added Part 2: four self-drawn PNGs plus
one data file per seat, shared, persisted by vote, referenced with
`![Alt](upload:<12 hex>)`, and the mirror/download experiments to log with
timestamps. All three seats already mirrored (`read_mirror.on: true`, 1 GiB
each) - a fact learned at start, not chosen.

The orchestrator watched from outside with a per-minute monitor
(head / docs / rev / `diverged` / open proposals / uploads per node) and read
the node logs; it intervened ONCE (Right's node restarted, §3 D9).

Legend: **[V]** verified by the orchestrator on the nodes or logs, **[A]**
reported by a seat and not independently re-checked, **[P]** pre-existing
before this day's build.

## 2. Did the fixes of rounds 1 and 2 hold?

| fix (round) | verdict in round 3 |
|---|---|
| interleaved proposal ids, two guards (1) | held - 130+ proposals, no collision; the seats even predict their next id from the roster stride (and guessed wrong once in public, §5 R7) |
| deep tie-break / reorg (1) | not exercised - the majority chain never forked (`diverged` empty on every node all run) |
| seal pacing (1) | no burst problem observed |
| divergence detector `read_chain.diverged` / `status.chain_diverged` (1) | **failed for the case that occurred**: a seat cut off by a refused checkpoint is not a fork; the field reports contradiction, not silence (D9, R3) |
| patch channels stay writable, `note` on approve/decline (2, A1-A3) | held - all three name it the fix that matters at 2-of-3; 91/99/86 patch-channel messages per seat, no lost review |
| `create` op + warnings, open-path / rename / name-collision / foreign-rewrite (2, B) | held for the cases they were built for; two new edges: warnings are computed against the BASE, not the working copy, and `allow_warnings` disables all of them (R19) |
| `sealing` state, `held_for_secs`, vote reply (2, C) | `approvals` still counts local signatures only (G12 re-observed, R11/R24) |
| tool text, `superseded` as a state (2, D) | `superseded` now READS as a state, but it means "the base moved", not "dead" (R20) |
| `dry_run`, `with_patch`, `supersedes` in `wiki_edit` (2) | the instruments of the round: no proposal refused for form all run (R16, R26) |

Against round 2 (four hours, 75 proposals, 125 pages, no fork): round 3
in 74 minutes filed 70 proposals (Left 23, Center 28, Right 19), 61 of them
applied on the majority chain, and took the wiki from 125 to 212 pages with
`wiki_health` 0/0/0 and `files` 0/0/0 at the end - and lost one seat to a
defect the earlier rounds could not reach, because no checkpoint had ever
sealed under load before (round 2's cut at 59 sealed after the run).

## 3. Findings on the engine and its surfaces

### D9 [V] CRITICAL - a checkpoint cut partitions a seat, and nothing says so

Timeline (UTC): head 94 - Left and Center read `wiki_rev` 24, Right 26 at
the SAME chain height (the folded base is not a pure function of the chain).
15:09:39 Center's engine AUTO-proposed a compaction checkpoint (`checkpoint:
auto-proposed a compaction checkpoint len=38`); sealed at 97 by Center and
Right. 15:09:45 and :46 Right REFUSED the sealed block twice:
`refused a chain block height=97 error=the shared memory base this node holds
(d947f7bf…) is not the committed one (28d28805…)` (`chain/wiki_base.rs::summarize`).
From then on Right sat at 96 and dropped every approval as "implausible
future height" (11 of them); Left and Center ran to 126 with `wiki_rev`
reset to 0 (D3). `read_chain.diverged` and `status.chain_diverged` stayed
EMPTY on all three nodes for the rest of the run (Left counted: 27 blocks
signed Center+Left, one - the checkpoint - Center+Right).

Evidence gathered before the intervention: all 197 pages read
byte-identical on Left and Right (`wiki_get` on every path), so the TREES
agree while the commitments do not - Right, a full holder since cut 59, is
verified as if it were a suffix holder of the 96-base. Right's own
`wiki_changes` afterwards reported `base = 28d28805…` (the committed hash);
Left's reported `base = 2334dce1…`. The co-sign check let Right sign a
commitment it then could not reproduce.

Orchestrator intervention 17:15-17:17 local: Right's node killed and
restarted, workspace reopened. It did NOT heal: head 96 / rev 26 / 40
blocks / `diverged: []` for the remaining 30 minutes; block 97 was never
re-offered or refused again above INFO. Transport recovered at 15:37Z
(peers' `last_seen` 1250 s → 1 s, 41 chat messages and 18 proposals arrived
at once), the chain did not (R9). Right's approvals cast at height 97 were
silently dropped by the majority (R24) while its chat and proposal TEXTS
arrived - Right's finale (225) was applied on the majority chain by the
other two.

What the seats used instead of the detector (R3): the `⚖` decision lines in
their own chat naming decisions their node did not hold, a wave of
`superseded` without a path conflict (R1), a standing head under running
decisions, and `read_members.last_seen` ageing linearly. Mr. Right posted
`DIVERGENZ … seit 15:15:35Z (Block 97)` at 15:22Z under rule 2; Left
confirmed it at 15:34Z with the signer count.

### D3 [V] `wiki_rev` restarts at 0 on a cut

At head 94 all three read rev 24; after the checkpoint at 97 Left and
Center read rev 0, Right (at 96) 26. `wiki_changes since_rev`, the seats'
HEAD sync lines and the pane's per-rev caches all key on it; two HEAD lines
across a cut read as a divergence and are none. Only `wiki_changes`' text
mentions the re-base. Fix direction: carry the cut in the rev (e.g. `(cut
height, patches since)`) or never reset.

### R1 [A] HIGH - the cut supersedes every open wiki proposal

15:14-15:16Z five open proposals of three seats (202, 203, 205, 207, 213)
flipped to `superseded` at once and lost their votes - none shares a path
with the last sealed wiki block (194), the same edit lists pass `dry_run`
clean. The base REVISION moving is what minted `superseded`. Two traps ride
on it (R20): the proposal keeps `state: proposed` in `list_proposals` (only
`read_proposal` carries the flag) and the proposer's OWN signature is
dropped; a caller counting votes waits forever. And a proposal can be
`superseded: true` and still seal (238, 244) - the flag means "base moved",
not "dead". Re-proposing needs `allow_warnings` because "path in open
proposal N" counts the caller's own dead proposal (R19).

### R12 / R17 [A] HIGH - restart loses the proposer's signature and the attribution

After the reopen Right's persist proposals 198/201 read `approvals: 0/2`,
every seat `open`, although "the proposer's own signature is the first of
m". Left's node lists all 22 stored proposals as `by: "Mr. Left", mine:
true` - including #96, provably Right's (Left declined it in its patch
channel) - with `approvals: 0` and every vote `open` while the patch
channels carry "declined by Mr. Right / Mr. Center"; Center's node lists
36 rows, every one its own (own-seat history only, live queue complete).
`payload.by` on a files proposal stays correct while the top-level `by`
lies. Explains D4 (differing pending counts after the restart) together
with R2: four of Right's round-2 proposals (3, 55, 102, 111) stayed
`proposed` for a day because they were pure CREATE patches whose paths were
long in the base - the superseding check looks at changed paths only.

### D10 [V] the decision line's deterministic id trips the ingest's audit trail

`post_decision_summary` mints the `⚖ #id ✓ …` line with the SAME id on every
seat that tips ("D5: DETERMINISTIC id - whoever tips posts"); `net/ingest.rs`
(P5) treats a cross-AUTHOR duplicate as "either a bug or an attempt to
occupy a foreign id" and logs WARN: 57 such warnings per node in 40
minutes. Two rules that contradict each other; the audit trail cries wolf.

### D11 [V] after a kill, the MLS ratchet rejects the resends at ERROR level

After the restart Right logged 182 `ERROR openmls::framing::private_message_in:
Ciphertext generation out of bounds 801/817 … SecretReuseError` for the
peers' resent frames (the persisted ratchet lags the live one by up to 10
s); they stopped after the burst and fresh generations decrypted. The
2026-09-03 log-noise fix demoted the stale-frame WARNs; this path was missed.

### R11 / R24 [A] votes from the wrong height vanish without a word

Right kept approving from height 96/97 after the split; `Reply::Vote`
answered as if they counted (G12: local signatures), the majority nodes
dropped them silently - `read_proposal` on 209/198/201/222/260 showed
`Mr. Right: open` everywhere, even on 222 and 225, his own proposals.

### The file plane (all NEW this run)

- R21 [A] HAZARD: a shared file replaced on disk after `share_file` is
  invisible - three persisted files overwritten in the exchange folder still
  read `available: true, local.kind: own, temporary: false` and the OLD size;
  nothing re-hashes, nothing warns (the transfer plane refuses at fetch time,
  the GUI's `ReadUploadBytes` at read time). A share must be immutable from
  `share_file` on; the tool text does not say so.
- R10 [A] HIGH: the same reference reads CLEAN on one chain and TEMPORARY on
  another (Center approved Right's finale with "all four references are
  persistent" - on Right's chain two were temporary because persist votes
  198/201 never sealed there); both nodes report `wiki_health.files` 0/0/0.
  A hygiene value is local truth and the reply does not say which chain
  (height, base hash) it was measured on.
- R18 [A] Download vs mirror: `download.percent` sits at 0 / `requested`
  for ~70 s while `local.kind` already reads `partial`; both paths fill the
  same piece store, so "was it the mirror or my download" cannot be told;
  `mirrors` counts only complete holders. MIRRORED bytes live in
  `read_mirror.dir`, not the exchange folder: an agent wanting to SEE a
  mirrored file must `download_file`, which fetches over the network
  although the bytes are local.
- R5 [A] A reference whose only match is TEMPORARY passes `wiki_edit`
  silently (by the plan) while `resolve_upload` says `temporary: true`; all
  three seats measured it, Center declined two such proposals (189, 212) by
  hand. The only guard is a reading seat, 2 min 24 s later.
- R4 / R23 [A] Tool-text gaps: `propose files persist` takes the chat
  MESSAGE id (the grammar lives only under `read_uploads` / `resolve_upload`,
  `propose` names `files` in its enum alone); `availability`
  (`sharer-only` / `relay-held` / `gone`) is unexplained and reads wrong for
  a persistent file mirrored by all three seats (`sharer-only`);
  `mirror_held/of` count pieces, `mirrors` members; `unpersist` needs `at`
  (the share's `ts`), learned from the refusal; `read_chain` blocks carry no
  timestamp; the persist payload shows voters `key_b64`; `read_mirror.files`
  lists shares before `read_uploads` knows them (R14); `upload:` warnings do
  not tell a malformed hex from an unknown one.
- R22 [A] No reverse lookup "which pages reference this file" -
  `wiki_search` does not index `upload:` destinations, so an `unpersist`
  needs `wiki_get.files` on every page.
- R8 / R26 [A, measured] What worked: share → 64-hex checksum in 3-14 s;
  persist applied → `none` → `partial` → `mirrored` in 1-5 minutes without a
  call (piece cadence = `mirror_publish_interval_secs` 15); `resolve_upload`
  answers everything a reference needs in one call; `![alt](upload:<12 hex>)`
  resolved on the first attempt on every page; `wiki_get.files` reported
  name + state exactly as documented; `unpersist` verified end to end
  (`persistent: false`, fresh `expires_ts`, bytes stay); the dangling-hex
  warning is one exact line and refuses without `allow_warnings`.

### The wiki surface (carried over and sharpened)

- R19 [A] `wiki_edit`'s title/alias warning is computed against the BASE:
  an edit list that removes an alias and re-creates it in the same call is
  refused; `allow_warnings` then disables EVERY warning.
- R15 [A] `rename` does not repair incoming links and does not warn about
  links in the BASE (only in open proposals); `wiki_links {direction: in}`
  is mandatory and in no tool text.
- R6 / R25 [A] `wiki_health` sees spellings (`key_drift`), not a WRONG key
  (seven person pages with `year` instead of `notable_year`) nor a relation
  asserted in the wrong DIRECTION (13 reversed `authored_by` edges on
  Center's own round-2 pages, found by hand). A per-`type` props check and a
  "predicate contradicts the page's type" group are missing.
- R13 [A] Text contradictions: `propose` says a surface whose feature is
  not enabled is refused while `files` is proposable with
  `status.features = ["memory"]`; `wiki_health` answers `wiki_rev: 0` on a
  125-page wiki after a cut without saying why; `read_chain`'s top block after
  a cut is the checkpoint with `proposal_id: 0`.
- R7 [A] Still open from round 2: `withdraw` takes no `note` and answers a
  bare ack; the proposal id exists only after `propose`; `wiki_list {prefix}`
  filters by folder, not string; `wiki_health` / `wiki_neighbors` answer a
  hard error while the index rebuilds after a multi-page apply.
- R16 [A, positive] `wiki_edit {dry_run: true, with_patch: true}` answers
  "may I write against an uncertain base" - Right proved his finale disjoint
  from the divergence with it and was right to file it.

## 4. Findings on the build (from the exhaustive test run before the round)

- D1 [V][P] `scripts/gui_walk.py` cannot found a republic over MCP since the
  2026-08-26 audit: the relay step is fixed (5fca28b8: the dev relay is
  pre-confirmed in the config), the seed step (`confirm_seed_backup` needs
  `join.seed`, never serialized) is not fixable in the script. Recorded in
  `docs/reviews/known_debt.md`; phases 3-5 of the walk unverified since the
  audit.
- D2 [V] Two molt-ui GUI tests fail in the NORMAL profile (generated
  window) and pass in the live-preview flavour:
  `a_pending_base_replaces_the_empty_state` (`find_by_accessible_label`
  finds no element with `Strings.mem-empty`) and
  `the_authoring_modals_fit_the_window_at_every_font_size`
  (`find_by_element_type_name("ConfirmModal").count() != 1`). Both fail in
  the testing backend's ELEMENT LOOKUP, not in behaviour; both predate the
  day's round and were only ever run in live-preview (hypothesis [P]; a
  window build at `df798858` settles it). CLAUDE.md's "part of the ordinary
  suite" is true for the live-preview flavour only.
- D5 [V][P] `confirm_seed_backup`'s text still says "(create.seed /
  join.seed)" - an impossible flow for an MCP client.
- D6 [V][P] `cargo build -p molt-app --features ui-testing` is a SECOND full
  window build (10m44s) - the feature changes the window crate's unit hash;
  CLAUDE.md does not say so.
- D7 [V] `scripts/check-doc-refs.py` scans IGNORED files at the repo root
  (an untracked `deleteme.md` makes it exit 1 in the main checkout).
- D8 [V] `cargo clippy -p molt-ui --all-targets` in the normal profile
  check-builds the 400k-line window module (~11 GiB); killed at 16:33 to
  protect the round's nodes. The live-preview clippy is clean; the
  normal-profile clippy of molt-ui/molt-app is unverified for the day.

## 5. Final state (17:47 local, majority chain = Left and Center)

| measure | value |
|---|---|
| chain head / wiki docs | 126 / 212 on Left and Center (`diverged` empty); Right 96 / 197 |
| `wiki_health` | dangling 0, orphans 0, key_drift 0; `files` dangling 0 / temporary 0 / ambiguous 0 (Right: orphans 15, files 0/0/0 on its own base) |
| proposals filed this round | 70 (Left 23, Center 28, Right 19); applied on the majority chain 61; Right's four late ones open forever on its chain |
| declines | Center 3 (one wrong edge direction, two references to a temporary file), Left 1 (a wrong curve), Right 0 - every decline repaired and re-filed within minutes |
| renames / foreign-page sections / disputes | 3 / 9 / 6, all in-links repaired by hand |
| files | 18 shares, 15 persistent (5 per seat: four PNGs + one CSV/data file each), 3 unpersisted v1 pictures; every persistent file referenced from at least one page, `wiki_get.files` reading `local` on the sharer, `mirrored` elsewhere |
| pictures | 12 PNGs drawn with PIL from the wiki's own facts (protocol stacks, delivery paths, succession chains from `wiki_neighbors … supersedes transitive`, timelines and bar charts from the `year` / `first_released` headers and `wiki_search` facets), 800-1600 px |
| messages | ~500 (Left 123, Center 124, Right 47 in this round's window; patch channels 91 / 99 / 86, `sync` 14 / 8 / 6) |
| ontology | version 5 (Center): §9 the picture and file grammar |
| relations | 33 keys; `type` 212 (standard 59, implementation 49, person 46, event 14, organization 13, primitive 13, protocol 12, overview 5, meta 1); `authored_by` 56-68, `implements` 57, `depends_on` 49, `disputed_by` 21, `supersedes` 11 |

## 6. Discussion agenda (the user's decision; ordered by damage)

1. **D9 - the fold commitment and the checkpoint verification.** Why did
   Right's `wiki_rev` run two ahead of the others at equal height, why does
   `summarize` compare a full holder's held base against the new
   commitment, and why did the co-sign pass while the apply failed? Plus:
   should an auto-proposed cut ever seal while ANY seat's fold is unknown
   (the proposer could require a fold hash from every co-signer first)?
2. **The silent partition.** `diverged` must also report "a member has not
   co-signed for N blocks" / "peers are N blocks ahead" (Left's and Right's
   proposal), and a vote cast at the wrong height must be answered with a
   refusal, not counted locally (R11/R24, G12).
3. **The cut's collateral: D3 + R1 + R20.** A monotonic `wiki_rev` (or the
   cut height beside it), `superseded` split into "re-base needed" vs
   "conflicts", the flag in `list_proposals`, and the proposer's signature
   surviving a re-base.
4. **Restart durability: R12 + R17.** The proposal store must keep
   authorship and the own signature across a reopen.
5. **D10 + D11.** The decision line needs an id the ingest recognizes (or a
   per-author id with content dedupe); openmls's replay noise stays out of
   the log.
6. **File plane semantics:** `download_file` serves from the local mirror
   first (R18), a file replaced on disk is detected (R21), `wiki_edit` warns
   on a TEMPORARY-only match (R5, the user's Q2 revisited), the `availability`
   word and the piece/member counts get one line of text each (R4), a
   reverse lookup for file references (R22), the `at` of `unpersist` (R23).
7. **Wiki hygiene:** warnings against the working copy (R19), `rename`
   repairs or refuses on base in-links (R15), a per-type props check and a
   direction check (R6/R25), `withdraw` with `note`.
8. **Build and docs:** D2 (find out whether the two normal-profile GUI tests
   ever passed), D1/D5 (the walk and the seed text), D6/D8 in CLAUDE.md, D7.

Discussed with the user the same evening: 1-7 approved as proposed (6 with
"a reference to an unknown OR temporary file is refused"); 8 answered by a
concept change - ADR-0007, the agent operates the machine, founding over
MCP comes back. The execution plan is
`docs/reviews/mcp_agent_friction_fixes_round_3.md`.
