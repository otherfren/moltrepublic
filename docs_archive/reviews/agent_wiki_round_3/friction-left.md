# friction.md - Mr. Left (seat A, port 4040), run 3, 2026-09-06

Legend: [NEW] = first reported in run 3 (files/pictures feature), [R2] = already
reported in round 2, [OK] = worked as documented.

## Log

- 16:31 `./mcp --list` returns 63.3 KB in ONE blob. [R2] There is no way to ask
  for a single tool's text; the wrapper has no `--list <tool>`. Cost: the whole
  text has to be paged through a file.
- 16:34 [NEW, severe] After the node restart the PROPOSAL ATTRIBUTION is
  broken: `list_proposals` and `read_proposal` report `by: "Mr. Left"` and
  `mine: true` for ALL 22 submissions - also for #96, which demonstrably
  was submitted by Mr. Right (patch channel 96, first message from
  Mr. Right; I myself wrote DECLINE there). Cross-check: Mr. Right reports
  in the group chat "four proposals ... all four mine", my node sees only
  two open ones and takes both for mine. Also lost: `approvals` is 0
  everywhere and `votes` is "open" everywhere, although the system lines
  in the patch channels read "declined by Mr. Right" / "declined by
  Mr. Center". Whoever needs the authorship has to read the patch
  channel - the field lies.

## Part 2 - Files and images (everything [NEW] in this run)

- 16:41:38 `share_file {"name":"left-nostr-verschluesselungslinie.png"}` ->
  `{"reply":"ack"}` [OK]. The row appeared with a 64-hex `checksum` at
  16:41:47 in `read_uploads`, i.e. **9 seconds** for 87814 bytes. The tool
  text says "async - poll read_uploads" but gives no order of magnitude;
  9 s is the measured value.
- [NEW] `propose {"surface":"files",...}` works ALTHOUGH `status.features`
  lists only `["memory"]`. The tool text of `propose` says "Proposing on a
  surface whose feature is not enabled is refused (status.features lists
  the enabled set)" - so `files` is not a charter feature but always
  there. This is stated nowhere and I sent the call expecting a refusal.
- [NEW] The payload of a persist submission carries `by` CORRECTLY
  (`payload.by = "Mr. Center"`), while the top-level field `by` of the
  same submission wrongly says "Mr. Left". Whoever needs the authorship of
  a file submission reads `payload.by`.
- [NEW] The persist payload contains `key_b64`, `root` and `pieces`. The
  file's key is thereby in the vote - harmless within the republic, but no
  tool text says that approving a persist submission means seeing the file
  key.
- [NEW] EXPERIMENT "download before the mirror", fully measured
  (B-protokollstapel.png, Mr. Center, 102817 bytes, 3 pieces):
    16:42:34.2  before approve   local=none  temp=true  avail=sharer-only mirrors=1 held=0/3
    16:42:34.3  approve 130      -> applied, approvals 2/2, head=60, immediately
    16:42:34.5  after approve    local=none  temp=FALSE avail=sharer-only  (temp flips in <0.2 s)
    16:42:34.5  download_file    -> {"reply":"ack"}
    +10.2 s     local=partial, avail=relay-held, download.percent=0 phase=requested
    +72.2 s     held 1/3, percent 42, phase=transferring
    +88.4 s     held 2/3, percent 85
    +104.7 s    local=DOWNLOADED, mirrors=3, held 3/3, percent 100, phase=done,
                path=<exchange-left>/B-protokollstapel.png
  Three findings from this:
  1. `download.percent` stays at 0 and `phase` at "requested" for 70
     seconds, WHILE `local.kind` already reports `partial` and
     `availability` has jumped from `sharer-only` to `relay-held`. The
     download's progress display lags the actual piece acquisition by
     more than a minute.
  2. The assignment wanted to separate "none -> partial -> mirrored" from
     the download. That is NOT possible: both fill the same piece store,
     and because I explicitly called `download_file`, the state ends as
     `downloaded`, never as `mirrored`. The question "was it the mirror
     or my download" cannot be answered with these tools.
  3. `mirrors` only jumps from 1 to 3 at the end - the counter counts
     only members with the COMPLETE series, a partial holding does not
     appear in it at all. `availability` knows at least the words
     `sharer-only` and `relay-held`; what exactly they mean, no tool text
     says.

## Tool findings part 1 (wiki)

- [NEW] `wiki_edit` checks the name warning against the BASE, not against
  the working copy. Submission 176 removes in edit 1 via `set_props` the
  aliases "Merkle Search Tree" and "MST" from
  `standards/atproto-repository.md` and in edit 3 creates the page
  `primitive/merkle-search-tree.md` with exactly these names. The tool
  text says "edits apply IN ORDER to a working copy of the current
  base" - the warning
  `title "Merkle Search Tree" already names standards/atproto-repository.md
  - both pages lose it` appears nevertheless, three times, and forces
  `allow_warnings: true`. A real warning can then no longer be told apart
  from a stale one in the same call: `allow_warnings` switches off ALL of
  them, there is no "I know this one".
- [NEW] References `upload:<hex>` are free-form. All four cases - 11
  digits, 12 digits, 65 digits and `zzzzzzzzzzzz` (not hex) - yield the
  same warning `file reference unresolved: upload:<whatever>`. The
  documented range is "12..=64 hex digits", but a typo in length or
  alphabet cannot be told apart from a valid but unknown checksum.
- [NEW] A reference to a still TEMPORARY file does not warn, neither in
  `dry_run` nor in the real call (measured 16:51:34, submission 164 on
  `left-at-protocol-weg.png`, `resolve_upload` said `temporary: true` at
  the same moment). This is documented and it is the right decision - but
  it means that the only warning before a page that points into the void
  after the chat window is `wiki_health.files.temporary`, and nobody who
  is currently writing reads that.
- [NEW] EXPERIMENT "mirror without download" (control group, 6 foreign
  files), state 16:58:20:
    B-ap-zustellweg.png            local=mirrored   held 2/2
    B-zeitachse-segment.png        local=mirrored   held 3/3
    B-implementierungen-balken.png local=mirrored   held 2/2
    B-implementierungen.csv        local=mirrored   held 1/1
    mr-right-abloeseketten.png     local=mirrored   held 3/3
    mr-right-kryptoschicht-stapel  local=mirrored   held 3/3
    B-protokollstapel.png          local=DOWNLOADED held 3/3   (the one I pulled)
  THE DIFFERENCE that no tool text states and that is practically the most
  important: mirrored bytes live in
  `~/.moltrepublic/mirror/<workspace-id>/` (read_mirror.dir), NOT in the
  exchange folder. Of six foreign files exactly ONE is in the exchange
  folder - the downloaded one. An agent that wants to LOOK AT a foreign
  file has to call `download_file`, even if `local.kind` already says
  `mirrored`; "the node has the bytes" and "I can get at the bytes" are two
  different statements.
- [NEW] `wiki_edit` with `op: rename` does NOT repair IN-LINKS. Measured
  2026-09-06: a pure rename of `standards/lexicon.md` to
  `standards/atproto-lexicon.md` yields with `with_patch: true` exactly
  this patch -
    `similarity index 100% / rename from ... / rename to ...`
  - and `paths` names only the one file. The five pages that carry the
  path spelled out have to be fixed up by the caller, otherwise five
  edges dangle. `wiki_links {"direction":"in"}` is the only way to find
  them.
- [NEW] `wiki_list {"prefix": ...}` filters by FOLDER, not by string.
  Measured: `standards` -> 56, `standards/` -> 56,
  `standards/atproto-` -> **0**. The tool text says "only paths under
  this folder" and is thus correct - the parameter name `prefix` suggests
  the opposite nonetheless, and I planned a rename with a justification
  that failed on this.

## Governance and chain

- [NEW] A 2-of-3 republic can LEAVE A SEAT BEHIND without any node
  reporting divergence. Measured 17:33 on my node: of 28 blocks above the
  genesis, **27** carry the signers (Mr. Center, Mr. Left) and **one**
  (height 97, the checkpoint) the signers (Mr. Center, Mr. Right). The
  highest block with Mr. Right's signature is 97; he himself reports his
  chain standing at 96, so he SIGNED the cut and did not APPLY it.
  Meanwhile: `read_chain.diverged` = [] and `status.chain_diverged` = []
  in EVERY reading of this run. This is not a detector error - there is no
  fork, Center and I are a valid quorum. But whoever reads `diverged` as
  "all fine" never sees this case. A signal of the kind "member X has not
  co-signed for N blocks" is missing.
- [NEW] Cross-check that rules out the obvious explanation: these are not
  blocks that were sealed before his vote arrived. `read_proposal` on 209,
  198, 201, 222 and 260 shows `Mr. Right: open` for ALL of them - also for
  **222, his OWN proposal**, whose proposer signature is, per the tool
  text, the first of m. His governance messages have not reached my node
  since 15:15:35Z, while his CHAT arrives (DONE message 17:31:22,
  `read_members` presence 0 at 17:33:02). Chat and governance ride the
  same broadcast stream, and only one of the two arrives.
- [R2, confirmed] `approvals` counts only the signatures of THIS node.
  After `approve` it often reads 1 and the block seals seconds later with
  2 - this is not a bug, but it makes the number useless as a progress
  display.
- [NEW] The CHECKPOINT resets `wiki_rev` to 0 and changes the `base` of
  `wiki_changes` (`truncated: true`). Two HEAD lines across a checkpoint
  are therefore NOT comparable - by the assignment's sync rule this looks
  like a divergence and is none. The checkpoint at height 97 came about
  without my vote; I was not asked and noticed it only from the reset rev.

## What worked well [OK]

- `wiki_edit` with `dry_run` before every batch: 16 submissions, not a
  single one rejected for a form error. The warning "alias X already
  names Y - both pages lose it" found a real name conflict twice before it
  stood in the wiki.
- `supersedes` in `wiki_edit`: 212 -> 242 in one call, without a rejected
  version staying open.
- `approve` with `note`: the reason lands BEFORE the vote in the patch
  channel. At 2 of 3 the window between first vote and decision is a
  single foreign action wide - without this parameter at least five
  justifications would have fallen into an already closed channel in this
  run.
- `share_file` -> `read_uploads` -> `propose files persist` ->
  `resolve_upload`: the chain worked five times without surprise, hash
  duration 9 to 14 seconds for 6 KB to 145 KB.
- `wiki_links {"direction":"in"}` is the only tool with which a rename can
  be fixed up cleanly at all.

## What was missing

- A way to read the authorship of a submission reliably. `by` and `mine`
  are wrong after a restart, `payload.by` exists only on the surface
  `files`, and for wiki submissions all that remains is: read the patch
  channel and see who wrote the first message.
- A search for a checksum. `wiki_search "b8ecea4ac949"` finds nothing,
  although the string could stand in the text as an `upload:` reference.
  Before an `unpersist` one has to read `wiki_get.files` on EVERY page
  that carries a file at all - there is no reverse question "which pages
  name this file".
- A signal for a seat that has fallen behind (see above).
- [NEW, more precise] The diagnosis of the divergence can be split into
  THREE channels, all measured on 2026-09-06:
    Chat            arrives    (DONE message Mr. Right 17:31:22,
                                read_members presence 0 at 17:33:02)
    Proposal texts  arrive     (his proposal 225 readable at mine at
                                17:40, with patch, countable, signable)
    Signatures      do NOT arrive (0 of 29 blocks since 15:15:35Z,
                                including his OWN proposals
                                222 and 225, whose proposer signature
                                is, per the tool text, "the first of m")
  Only ONE of the three paths has failed, and it is the one that no tool
  observes. `read_members` reports him as online, `read_chain` reports
  `diverged: []`, `list_proposals` shows his proposals - all green, and
  yet he has not co-signed for 29 blocks.

## Final state 17:47

- Chain: h=126, last=225, docs=212, rev=19, diverged=[],
  status.chain_diverged=[]
- wiki_health: dangling 0, orphans 0, key_drift 0,
  files {dangling 0, temporary 0, ambiguous 0}
- Exchange folder: 5 own files (4 PNG, 1 CSV) plus
  B-protokollstapel.png, the only FOREIGN file on disk - and the only one
  I fetched via download_file. The other six foreign ones lie mirrored in
  ~/.moltrepublic/mirror/<workspace-id>/, not here.
- All open proposals are decided, four measurements 30 s apart: open 0.
