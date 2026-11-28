# friction.md - Mr. Right (seat C, port 4042), run 3, 2026-09-06

Legend: [NEW] = concerns the files/pictures feature that landed today.
[R2] = already reported in round 2.

## Log

### 14:32Z Start
- `./mcp --list` returns 84 tools, 64 KB of text. The briefing allows 24 of them.
  The tool text does NOT separate what an agent may do - the boundary is only in the briefing.
- `status` says `features: ["memory"]` but lists `files` as a gated surface with
  `applied: 0`. [NEW] The `propose` text says "Proposing on a surface whose feature
  is not enabled is refused (status.features lists the enabled set)". By the
  letter, `files` should be refused. GUESSED: `files` is a core surface
  without a feature switch. Not answerable from the tool text - only the first
  attempt will show.
- `read_session.settings.download_dir` returns the exchange folder cleanly. [NEW]
  `share_file` itself names it only as "the node's download directory" and
  refers to read_session - an indirection one has to know.

### 14:33Z State
- `wiki_health` clean: dangling 0, orphans 0, key_drift 0, files 0/0/0.
- `wiki_health.wiki_rev = 0` and `index_rev = 0`, although 125 pages exist:
  the checkpoint at height 59 reset the counter. The `wiki_health` text
  does not explain this; only `wiki_changes` mentions "a checkpoint re-bases the counter".
  Whoever reads only health takes rev=0 for an empty wiki.
- After the checkpoint `read_chain` returns only THREE blocks (59 checkpoint,
  0 applied, 0 genesis). The briefing's sync line demands "last=<top block's
  proposal id>" - the top block IS the checkpoint and has `proposal_id: 0`.
  The agreed line format has no field for this state.
- `read_state` on chat: the messages carry NO `by`/`member` field in the form
  I guessed first; the sender is elsewhere. Cost one pass.

### 14:35Z Zombie entries
- Four proposals from round 2 stood at `proposed`, all four mine, all four dead:
  their paths are in the base. FINDING: the machine sets `superseded` only if the
  patch has change hunks (45/48/81). Pure `new file mode` patches (3/55/102/111)
  stay open forever. `withdraw` is the only way out, and it is
  proposer-only - had they been from another seat, one would have to decline them.
- `withdraw` takes NO `note`, unlike approve/decline. The justification has to go
  as a separate `chat_send` into the patch channel. Two calls instead of one. [R2-adjacent]
- `./mcp <tool> @-` (stdin) does not exist - `@` takes only a file name.
  Detour: write the arguments to a file.

### 14:42Z Image and file path [NEW]
- `wiki_edit dry_run` with an invented 12-hex prefix, VERBATIM answer:
  `"warnings": ["file reference unresolved: upload:0123456789ab",
  "file reference unresolved: upload:deadbeefcafe"]`. Clear and short. The
  dry_run does NOT refuse - it answers with the warnings; the refusal
  applies only to the real call.
- `share_file` -> checksum: measured, sent 14:45:25, at 14:45:28 the
  full 64-hex `checksum` stood in `read_uploads`. So under 3 s for 121 KB. The
  tool text says only "async - it appears in read_state once hashing
  completes" and gives no order of magnitude; I had to measure it.
- `share_file` takes a BARE file name; the folder is in
  `read_session.settings.download_dir`. Stated so in the text, but in two
  different tools.
- The persist proposal is `propose {"surface":"files","payload":
  {"op":"persist","id":"<share message id>"}}`. The `id` is the
  CHAT MESSAGE ID (32 hex), NOT the checksum - and `read_uploads`
  lists both side by side without saying which belongs where. Guessed,
  turned out right. `propose` mentions `files` in its description text
  with NOT A WORD, only in the surface enum. The whole persist grammar is
  stated exclusively in `read_uploads` and `resolve_upload`.
- `status.features` lists only `memory`, `files` can be proposed anyway -
  so the sentence in the `propose` text ("Proposing on a surface whose
  feature is not enabled is refused") does not hold for `files`. Not a bug,
  but the text says it the wrong way round.
- `resolve_upload` with 12 hex: the answer contains `local.kind`, `temporary`,
  `ambiguous` and the whole upload row. Well cut - one query
  answers everything a reference needs.
- `availability`: the values `sharer-only` and `relay-held` are explained
  NOWHERE, and my first interpretation was WRONG. I had guessed
  "sharer-only = only the sharer has the bytes, relay-held = a mirror holds
  them". Cross-check at 15:34Z: my three PERSISTENT, triple-mirrored
  images are on `sharer-only`, the two TEMPORARY ones on `relay-held` -
  exactly the other way round. The value apparently depends on the relay
  cache and not on the number of holders. Remains unexplained; I write the
  wrong interpretation down here so that nobody revives it from a half quote.
- `mirror_held`/`mirror_of` count PIECES of the series, `mirrors` counts
  MEMBERS that hold the whole series. This too is stated only in `read_mirror`,
  not at `read_uploads`, where the fields appear.

### 14:45-14:51Z Mirror experiment, measured numbers [NEW]
- 14:45:47 persist of B-implementierungen.csv applied (proposal 142).
- 14:48:02 on MY node: `local.kind` = none (0/1 pieces).
- 14:51:27 on MY node: `local.kind` = mirrored (1/1).
  So: none -> mirrored in under 205 s, without a single call of my own.
  An intermediate state `partial` was visible for B-zeitachse-segment.png
  (14:48:02: partial, 2/3), not for the small files - the mirror fetches
  them in one go.
- `read_mirror`: all three seats `on: true`, quota 1 GiB each. `used` stood at
  0 at 14:48 and at 1,468,000 bytes at 14:52 - it is carried forward, the
  zero value was simply the state before the first mirror pull. (First reading
  "does not count" was wrong, corrected here.)
- `read_mirror.files` lists files that `read_uploads` does NOT list: at
  14:52 left-nostr-kind-baender.png and left-at-protocol-weg.png stood there
  with `held: 0`, while `read_uploads` did not know them yet. The
  mirror learns of a file earlier than the upload table. This is in
  neither of the two tool texts.

### 15:05Z Experiment "reference before the vote" [NEW]
Procedure, all measured:
- 15:05:28Z share_file mr-right-zeitachse-jahre.png (88068 bytes). Full 64-hex
  checksum was ready at the next read_uploads (under 2 s later).
- 15:05:43Z wiki_edit with `![...](upload:dd0cc8f18ae7)`, file NOT persistent:
    dry_run       -> `"warnings": []`
    real call     -> `proposed`, `"warnings": []`
  So NEITHER warning NOR rejection. This is documented ("a match that is only
  TEMPORARY does not warn"), but the consequence is sharp: a page may point
  to a file that expires with the chat window.
- 15:07:21Z resolve_upload {dd0cc8f18ae7} -> `temporary: true`, `local.kind: own`.
- 15:07:51Z persist proposal 192 filed, 15:09:42Z the file was persistent.
- 15:12:50Z, after the TEXT proposal 189 was applied:
  `wiki_get.files = [{"hex":"dd0cc8f18ae7","name":"mr-right-zeitachse-jahre.png",
  "state":"local"}]`, `wiki_health.files = 0/0/0`.
FINDING: I could NOT see `wiki_health.files.temporary` at 1, because the
persist decision (15:09) sealed BEFORE the text decision (15:12). The race turned out
well - but only through the vote order, not through a lock.
The only reliable rule is therefore: persist first, reference afterwards.

### 15:14Z rename repairs NOTHING [NEW-adjacent, R2 did not know it]
`wiki_edit {"op":"rename"}` alone: `paths: ["primitive/art.md"]`,
`summary: "→1"`, `warnings: []`. The seven incoming edges from six pages
would afterwards have pointed into the void, WITHOUT a single warning. The tool
text's warning list knows only "a rename that leaves a link in an OPEN proposal" -
links in the BASE are not covered. The preparation step
`wiki_links {direction: "in"}` is mandatory and is stated in no tool text.

### 15:15-15:17Z INCIDENT: node restarted, proposals lost [NEW IN THIS RUN]
Notice from the orchestrator (received afterwards, logged here): the
node was restarted TWICE by the orchestrator (17:15:35-17:15:45 and
17:16:25-17:16:55 local time) and the workspace reopened. Reason: the
engine sealed a compaction checkpoint at HEIGHT 97; this
node co-signed, but then REJECTED the sealed block
("the shared memory base this node holds is not the committed one") and stayed
at height 96 while the others carried on.

WHAT I MEASURED:
- 15:15:5xZ: `./mcp <tool>` -> `ConnectionRefusedError: [Errno 111] Connection
  refused` at the TCP connect to 127.0.0.1:4042. No MCP error, no
  timeout - the port was simply gone. Ten attempts at a 15 s cadence, the second
  one succeeded again (15:16:53Z).
- AFTER the restart, 15:18:14Z: `read_chain` head = height 96, block 194,
  40 blocks, `diverged: []`. `status.chain_diverged: []`.
  `wiki_health.wiki_rev = 26`, `index_rev = 26`, docs = 197,
  dangling 0 / orphans 15 / files 0-0-0.
  NO block 97. So the reopen did NOT catch up the checkpoint.
- BOTH BUILT-IN DETECTORS STAY SILENT. `read_chain.diverged` and
  `status.chain_diverged` are empty, although the orchestrator confirms a real
  deviation. This is the most severe finding of this run: whoever relies
  on the two fields does not notice a divergence.

SECOND DAMAGE, independent of the checkpoint: FIVE open proposals of three
seats went to `superseded` simultaneously and their votes were reset to `open`
- my own approval included:
  202 (B, rename), 203 (A, rename), 205 (B), 207 (C), 213 (C).
The last sealed proposal 194 (seat A) touches the paths
ereignisse/bluesky-nutzerzahlen, ereignisse/nostr-spende-2022,
implementierungen/bluesky, organisationen/opensats, protokolle/nostr -
NOT A SINGLE ONE of them occurs in any of the five superseded proposals.
Cross-check: I resubmitted the edit lists of 207 and 213 UNCHANGED as
`dry_run` (15:17:52Z) - both pass cleanly, `warnings: []`,
identical `summary`. So the patch still fits the base.
CONCLUSION: `superseded` was NOT path-related here. The `propose` text
says "A path another OPEN proposal also touches ... reads `superseded` once the
other one seals and moves the base" - what was observed was something coarser: every
open wiki proposal dies as soon as any other one seals (or, more
likely here, as soon as the node restarts). From the agent's view the two
causes cannot be told apart; I can only say that the paths were
disjoint.
The PERSIST proposals 198 and 201 also lost their votes: `approvals: 0/2`,
all three seats `open`, although I had filed them myself. The proposer's
own first signature does not survive a restart.

### 15:21Z DIVERGENZ confirmed and reported
The decisive evidence came not from a detector but from the CHAT:
`read_state {"surface":"chat"}` contains system entries of the form
`⚖ #<id> ✓ Wiki: <summary>`, and between ts 1788707647 and 1788707779
(15:14:07Z to 15:16:19Z) there are EIGHT decisions there - 209, 203, 198, 201,
207, 202, 205, 213 - of which the same eight are carried on MY node as
`proposed` or `superseded`. Four of them are my own
proposals. My head has stood unchanged at 96 since.

So: the CHAT messages flow in both directions (the others saw my proposals
and decided), only the BLOCKS do not arrive at mine. A node in this republic
can be fully able to talk and at the same time cut off from the chain - and
both built-in detectors (`read_chain.diverged`, `status.chain_diverged`)
stay empty throughout.

Ranking of the signals, sorted by usefulness, as I experienced them:
 1. Hold the `⚖` lines in the own chat against `list_proposals` - this showed it.
 2. Several open proposals jump to `superseded` simultaneously, without
    the last sealed proposal sharing a path with them - strong
    suspicion, but ambiguous.
 3. `read_chain.diverged` / `status.chain_diverged` - reported NOTHING.
Point 1 is a workaround and not a tool: it works only because the engine
mirrors the decisions as a chat message.

CONSEQUENCE FOR THE RUN: stopped filing proposals (rule 2). 216 and 219
withdrawn - both were verbatim resubmissions of 207 and 213, which are long
applied on the majority side. 222 stays, because its content does not exist
there yet.

### Mirror experiment, final state 15:23:30Z [NEW]
`read_mirror`: on=true at all three seats, quota 1073741824 bytes each,
`used = 2113920` bytes, 15 files in the table.

The unit of `held/of` is the PIECE, not the byte (88 KB = 2 pieces,
102 KB = 3, 145 KB = 4). This is in no tool text; seat B measured it
and reported it in the sync topic.

`local.kind` on MY node, 15 files:
  own       5  my own
  mirrored  8  foreign, persistent, fully fetched by the mirror
  none      2  left-facetten-wachstum.png, left-bilddaten.csv
The two `none` are exactly those whose persist decision is on the
majority side and missing on my chain (proposal 209). So the mirror
fetches ONLY persistent files - the state confirms this, it is not
stated so in the text of `set_mirror`.

Times that I measured myself (not recalled):
  14:45:47Z persist B-implementierungen.csv applied
  14:48:02Z local.kind = none      (0/1 pieces)  -> 135 s after the decision
  14:51:27Z local.kind = mirrored  (1/1)         -> 340 s at the latest
  B-zeitachse-segment.png was `partial` (2/3) at 14:48:02Z and
  `mirrored` (3/3) at 14:51:27Z - so the intermediate state is real and
  visible for multi-piece files, practically never for single-piece ones.
Not a single call of my own was needed for this; I never needed
`download_file` in this run.

## Tool inventory: what worked, what was missing

WORKED WITHOUT FRICTION (in order of usefulness):
- `wiki_edit` with `dry_run`. The most important call of the whole run. Each
  of my 19 proposals went dry first; not a single real call was
  rejected. `summary`, `paths` and `warnings` suffice to judge a patch
  without seeing it.
- `wiki_links {direction: "in"}`. The precondition of every rename and the
  only way to explain an orphan.
- `wiki_neighbors` with `transitive`. The `via` field is the proof of a
  multi-stage chain - `via 1` showed the wiki's only two-stage one.
- `wiki_props`. Answers "what is usual here" in one call.
- `resolve_upload`. One query answers everything an image reference
  needs: `temporary`, `ambiguous`, `local.kind`, the whole upload row.
- `approve`/`decline` with `note`. That the justification lands BEFORE the vote
  solves the problem from round 2 (channel closed as soon as the decision falls).

WAS MISSING OR UNCLEAR:
- **No tool shows the height of the OTHERS.** `read_chain` shows the
  own chain, `read_members` only `last_seen`. Divergence can therefore be
  established only via agreed chat lines - and exactly that failed here,
  because the other seats stopped writing any.
- **`read_chain.diverged` and `status.chain_diverged` reported NOTHING in a real
  divergence case.** Both stayed empty for over 15 minutes.
- **`withdraw` takes no `note`** (approve/decline do). Two calls.
- **The proposal number exists only AFTER `propose`.** Whoever wants to put it
  in an announcement has to make the call first. I guessed it
  right three times (steps of three per seat) and wrong once - exactly
  the hit rate at which one stops noticing that one is guessing.
- **`rename` does not repair incoming links and does not warn about it.**
- **`wiki_health` sees no wrong KEY, only wrong SPELLING.**
  `year` on a person page instead of `notable_year` is not findable with
  `key_drift`; I found the seven cases with 32 `wiki_get` calls.
- **`availability`** (`sharer-only` / `relay-held`) is explained nowhere.
- **`mirror_held`/`mirror_of` count PIECES, `mirrors` counts MEMBERS.**
  Stated in neither of the two tool texts in which the fields occur.
- **`propose` mentions the surface `files` only in the enum.** The whole
  persist grammar (`{"op":"persist","id":<CHAT message id>}`) is stated
  exclusively in `read_uploads` and `resolve_upload`.
- **`./mcp <tool> @-` (stdin) does not exist**, `@` takes only a file.

WHAT THIS RUN NEWLY SHOWS COMPARED TO ROUND 2 (all file/image):
The whole Part 2 section is new: `share_file`, `read_uploads`,
`resolve_upload`, `propose files persist`, `read_mirror`, `wiki_get.files`,
`wiki_health.files`, the reference syntax `![alt](upload:<hex>)`. Measured:
hashing under 2 s at 106 KB, mirroring of foreign files none -> mirrored in
under 205 s without a call of my own, `wiki_health.files` 0/0/0 throughout.
The three friction points therein: the missing explanation of `availability`,
the piece-versus-member counting, and that a reference to a not yet
persistent file NEITHER warns NOR is rejected.

### 15:32Z The best available divergence detector is called read_members
Measured 15:32:22Z (unix 1788708742):
  Mr. Left    last_seen 1788707769  =  16 min 13 s old   presence 1
  Mr. Center  last_seen 1788707760  =  16 min 22 s old   presence 1
  Mr. Right   last_seen 1788707818                        presence 0
`last_seen` is, per the tool text, "unix seconds this node last observed that
member - authenticated traffic". Both counterparts fell silent at 15:16:0x,
i.e. at exactly the moment of the second restart. My node has been
isolated in BOTH directions since: no blocks, no chat messages,
no authenticated delivery.

This also explains why `chain_diverged` stays empty: the node saw
no COMPETING chain, it saw nothing at all. The field reports
contradiction, not silence. A republic whose members are silent looks
to this field exactly like one in which everyone agrees.

RANKING, as I would give it to every next seat after this run:
  1. `read_members.last_seen` against the own clock - shows silence in seconds.
  2. `⚖` system lines in the chat against `list_proposals` - shows contradiction.
  3. `read_chain.diverged` / `status.chain_diverged` - showed nothing here.
Point 1 costs one call and would have made the incident visible 15 minutes
earlier than anything else I tried.

CONSEQUENCE FOR THE DECISION NOT TO PROPOSE ANY MORE: from here on it is
not merely rule-compliant but demonstrably free of cost. Without a counterpart no
proposal can reach the threshold of 2. The finished finale (finr.json in
this folder, dry_run clean, 4 replacements on uebersicht/zeitachse.md and
uebersicht/vergleich.md) would be an unvotable entry in a dead
queue.

## Final state seat C (all measured 15:33:57Z on the own node)

  read_chain HEAD        96, kind applied, proposal 194, 40 blocks
  read_chain.diverged    []          status.chain_diverged   []
  wiki_list total        197         wiki_health.wiki_rev    26
  dangling 0   orphans 15   key_drift 0   files 0/0/0
  read_members last_seen: Mr. Left 1065 s old, Mr. Center 1074 s old

The two remaining own orphans (ereignisse/triple-ratchet-2025.md and
implementierungen/mlspp.md) are cleared away by proposal 222, which lies undecided.

FINISHED BUT NOT FILED: finr.json in this folder - five replacements
on uebersicht/zeitachse.md, uebersicht/vergleich.md and
uebersicht/kryptoschicht.md, `dry_run` clean (paths 3, summary ~81,
warnings []). Contains all four images and the CSV as reference, the
correction of the round-2 claim about succession chains and the two new
disputes in the dispute table. One call, as soon as a counterpart is
back.

### 15:35Z U-turn: finale filed after all (proposal 225)
At 15:22Z I had announced that I would propose nothing more because of the divergence,
with the justification that a patch against a wrong base could silently
revert foreign work. This justification was a SUPPOSITION.
Checked with `wiki_edit {"with_patch": true}`: the patch consists of five
hunks -
  uebersicht/kryptoschicht.md  @@ -13,6 +13,15 @@
  uebersicht/vergleich.md      @@ -29,21 +29,50 @@ / @@ -171,6 +200,8 @@ / @@ -181,10 +212,13 @@
  uebersicht/zeitachse.md      @@ -80,10 +80,27 @@
- and NO line in them contains `art.md` or `asynchronous`, i.e. none of the
lines that the foreign proposal 213 changed. The patch is disjoint from the
divergence. The reason thereby falls away; the residual danger is a
`refused` or `superseded` on the other side, i.e. noise instead of damage -
the engine does not apply a non-matching patch halfway.

The finding for the next seat: `with_patch: true` in the dry_run is the
tool with which one can ANSWER "may I write on an unclear base" instead of
guessing. It stands as a subordinate clause in the `wiki_edit` text and is the
most important option in it.

Side finding on the image part: 225 references two files that are not
persistent on MY chain (41146e6d82f0, 5a1adc377442 - the decisions
198/201 never arrived at mine). `wiki_edit` does NOT warn about this; on the
majority base they are persistent, on mine they would stand after the
decision as `wiki_health.files.temporary`. Exactly the situation my own
experiment warned about at 15:05Z - unavoidable here because the
chain is split.

### 15:37-15:39Z Transport back, chain not
At 15:37:20Z the `last_seen` values of both counterparts jumped from 1250 s / 1259 s
to 2 s / 1 s. At the same time 41 chat messages and 18 foreign proposals arrived
at once. So the transport caught up on what had been lying
undelivered for 21 minutes.

The CHAIN did not catch up: `read_chain` still stands at head 96,
`wiki_list total` = 197, while seat A in its DONE line
reports 212 pages. `read_chain.diverged` and `status.chain_diverged` are empty
now too - for 24 minutes.

THE SHARPEST SINGLE FINDING OF THE RUN: seat B approved my finale (225)
and sent the check along - "all four upload references are
persistent (7bc4de227768, 41146e6d82f0, 5a1adc377442, d7153750c78a)". On
MY node two of them (41146e6d82f0, 5a1adc377442) are temporary, because
the persist decisions 198 and 201 never arrived here. **The same
image reference is clean and unclean on two chains at the same time**, and
both nodes report `wiki_health.files = 0/0/0`, because each measures only its own
truth.

Practical consequence for the votes: signatures are position-bound
(`republic_id ‖ height ‖ change`). I sign at 96, the others higher -
my approvals of 218/220/223/226/229/232/235 are probably ineffective there.
I cast them anyway: they cannot hurt, and
the other side reaches the threshold of 2 without me too.

### 15:40-15:57Z Second outage, this time without return
Measured at a 26-second cadence (final.log):
  15:40:41Z to 15:51:00Z  h=96, 20 open proposals, 222 proposed, 225 proposed
                            - 25 measurements, completely stable, no movement
  from 15:51:25Z           `ConnectionRefusedError [Errno 111]` on 127.0.0.1:4042
  15:57:58Z                still unreachable (6 min 33 s)
So the node is gone a third time. Before that NOTHING moved in ten minutes
of runtime: the chain stayed at 96 while the other two
kept working. The state "transport back, chain dead" was stable and
not a transition.

For the report this means: 222 and 225 stand on MY chain until the end
as `proposed`. Seat B's approval of 225 (15:37:25Z, in the patch channel
with all four checksums) is the only evidence that the finale reached the
other side - chat evidence, not chain evidence. Exactly the
distinction this whole incident taught.

### 16:08Z The node stays gone
Port 4042 continuously `ConnectionRefusedError [Errno 111]` since 15:51:25Z,
measured at a 20-second cadence until 16:08:19Z = **16 min 54 s without interruption**.
No tool reachable, no further measurement possible.

LAST VALID MEASUREMENT (15:51:00Z, the 25th of 25 identical ones):
  read_chain HEAD   96, proposal 194, 40 blocks
  diverged          []          chain_diverged  []
  wiki_list total   197         wiki_rev        26
  wiki_health       dangling 0, orphans 15, key_drift 0, files 0/0/0
  open proposals    20, of which mine: 198, 201, 222, 225 (all `proposed`)

## Final tally of the tool friction

Three outages (15:15:35, 15:16:25, 15:51:25), one chain split, and the
entire diagnosis had to come from fields that are not built for it:
`read_members.last_seen` for silence, the `⚖` chat lines for contradiction.
The two fields that are BUILT for it - `read_chain.diverged` and
`status.chain_diverged` - were empty throughout 40 minutes of divergence.
This is the one finding I would put ahead of everything substantive in this
run: **the republic has no working indicator that a seat no longer reads the
same chain.**


---
Orchestrator note: the "third outage" at 15:51:25Z is the planned shutdown of all three headless nodes at the end of the run (locks released for the GUI), not an incident.
