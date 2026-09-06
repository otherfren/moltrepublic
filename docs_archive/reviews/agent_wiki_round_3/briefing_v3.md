# Briefing v3: "Second Wiki Test" continued - complete the net, and put pictures in it

You are ONE seat of a live 2-of-3 republic (MoltRepublic: an encrypted DAO
with a threshold-voted wiki). Three agents, three seats, one wiki. Every
change is a proposal; 2 of 3 sign. This republic ALREADY holds the
knowledge net two earlier rounds built (125 pages, 59 chain blocks, four
ontology versions, a clean `wiki_health`). Your job is to CONTINUE it in
the same seat, complete it, and use the feature that landed today: a wiki
page can show pictures and point at files the republic keeps.

Tools: `./mcp --list` (the full tool text - read it ONCE, completely),
`./mcp <tool> '<json>'`, `./mcp <tool> @args.json`. Run everything from
your own folder (the one holding `./mcp`); keep `friction.md` there,
continuously. First call `./mcp status`: member == your seat, name ==
"Second Wiki Test". If `status` says no workspace is open, call
`./mcp open_workspace '{"id": "<the second-wiki-test id from read_session.workspaces>"}'`
and NEVER open any other workspace (a real republic sits beside it).
Use ONLY: status, read_session, open_workspace, read_state, chat_send,
list_proposals, read_proposal, approve, decline, withdraw, wiki_*,
read_chain, read_members, read_uploads, resolve_upload, share_file,
download_file, set_mirror, read_mirror, propose (surface `files` only).
Touch nothing in the repo.

## Seats and segments (unchanged from round 2)

- Mr. Left (port 4040): Nostr (NIPs, relays, clients, Marmot/NIP-EE) and
  Bluesky/AT (lexicons, PDS, relays).
- Mr. Center (4041): Matrix (MSCs, homeservers, clients, Megolm), XMPP
  (XEPs, servers, OMEMO), ActivityPub; owner of `meta/ontologie.md`.
- Mr. Right (4042): the crypto layer (Double Ratchet, MLS RFC 9420, Noise,
  OMEMO versions, OpenPGP), cross-cutting people and organisations, the
  timeline and the comparison tables.

Whoever created a page maintains it; everyone else adds a signed section
via `replace`, never a full rewrite (`content` OVERWRITES - use `create`
for new pages, `replace` for existing ones).

## Step 0 - read the state before you write (15 minutes, no proposals)

`wiki_list` (paged, all of it), `wiki_health`, `wiki_props`,
`meta/ontologie.md` (current version: it binds), the three
`uebersicht/*.md` pages, the last lines of the topics `sync`, `ontologie`,
`disputes` and the group chat (`read_state {surface: chat, channel:
{kind: topic, name: ...}}`), and `list_proposals` (nothing should be
open). Then write your COVERAGE GAP LIST for your segment: every protocol,
specification version, notable implementation, organisation and key
person that is still missing or thin (a page under ~600 bytes of body is
thin). Post it in topic `sync` as `GAPS <seat>: <n> pages, <m> thin`
with the first ten names. That list is your work order.

## Part 1 - complete the knowledge net

Breadth first, then depth. Research with WebSearch and WebFetch (load via
ToolSearch); sources under `## Quellen` on every page. Keep the ontology:
`type`, `title`, `tags`, `aliases`; predicates in the sentence as
`[[pred::Target|Display]]`; measured values as header numbers with the
unit in the key. Overviews and comparisons come from `wiki_search`
facets and `wiki_neighbors` (`predicate`, `transitive`), never from
memory. 3 to 5 pages per proposal.

The round-2 cooperation rules still hold, with these quotas for this run:
1. CHANNELS: group chat only for kickoff, PERSIST announcements (see Part
   2) and DONE; review remarks in the proposal's patch channel
   (`{"kind":"patch","id":N}`); topics `sync`, `ontologie`, `disputes`.
2. SYNC: after every fifth own proposal and at least every 10 minutes, ONE
   line in topic `sync`: `HEAD h=<read_chain top height> last=<top
   block's proposal id> docs=<wiki_list total> rev=<wiki_rev>`. Two rounds
   of differing lines → `DIVERGENZ <who> <since>` in `sync`, stop
   proposing, log every number. `read_chain.diverged` and
   `status.chain_diverged` are the built-in detectors: read them too.
3. VERIFY AFTER APPLY: `wiki_get` one page of every applied proposal and
   check `wiki_changes since_rev`.
4. FOREIGN PAGES: at least three signed sections on other seats' pages via
   `replace`; a page in an OPEN proposal waits.
5. DISPUTES: at least two real disputes (sources disagree), on the page as
   `[[disputed_by::…]]`, discussed in `disputes`, resolved by a `replace`
   citing both. At least two reasoned declines per seat over the run.
6. RENAMES: at least one rename of an own page; repair the in-links;
   `wiki_health` afterwards must show no new dangling entry.
7. HYGIENE: every 15 proposals (counted across seats) the next proposer
   reads `wiki_health` (INCLUDING its new `files` block) and fixes it in
   ONE proposal; numbers before/after go into the friction log.
8. PAYLOAD: the error message names the byte limit.

## Part 2 - pictures and files (NEW - this is what today's build adds)

A page names a file by its CONTENT: `![Alt](upload:<hex>)` renders a
picture inline, `[Text](upload:<hex>)` renders as a file link; `<hex>` is
the sha256 of a PERSISTENT shared file, full or a prefix of at least 12
digits. The tools: `read_uploads` (every share, `checksum`, `persistent`,
`availability`, `mirrors`), `resolve_upload {checksum}` (what a reference
names: `upload`, `ambiguous`, `temporary`, `local.kind` = none | own |
downloaded | mirrored | partial), `share_file {name}` (a bare file name
inside YOUR node's exchange folder - the path is in
`read_session.settings.download_dir`; the share posts asynchronously,
poll `read_uploads` for its row), `propose {surface: "files", payload:
{op: "persist", id: "<share id>"}}` (the vote that makes a share
permanent - only then may a page reference it; a `temporary` match is
shown by the pane as "not persistent yet"), `download_file {id}`
(fetches the bytes into your exchange folder), `set_mirror {on, quota_bytes}`
+ `read_mirror` (a seat that mirrors receives every persistent file by
itself), `wiki_get` now answers `files: [{hex, name, state}]` per page,
`wiki_edit` warns on a reference that resolves to nothing or to more than
one file, `wiki_health` gains `files: {dangling, temporary, ambiguous}`.

Your quota: at least FOUR pictures per seat, each a PNG you draw yourself
with Python/PIL (available: `python3 -c "import PIL"`) from the wiki's own
facts - a protocol stack, a message flow, a succession chain drawn from
`wiki_neighbors … supersedes transitive`, a timeline from the `year`
headers, a comparison bar chart from the `props` facets. Label every
picture inside the image with a title and "eigene Darstellung, <seat>,
2026-09-06"; 800 to 1600 px on the long edge; write it into your
exchange folder as `<seat>-<slug>.png` (your seat prefix, always - never
share a file you did not make). Also ONE non-picture file per seat (a
CSV or a text table the picture was drawn from), shared the same way and
referenced as a `[Text](upload:…)` link.

The flow, per file: draw → `share_file` → poll `read_uploads` until the
row carries its 64-hex `checksum` → `propose files persist` → announce
`PERSIST <seat> <name> id=<proposal id>` in the GROUP chat (the others
approve persist proposals promptly - it is the one thing the group chat
is for now) → after the vote applies, `resolve_upload` with the first 12
hex must answer `temporary: false` → reference it in the page it belongs
to (`replace` on an existing page, or `create`), the alt text saying what
the picture shows → after apply, `wiki_get` that page and record its
`files` block.

Experiments to run and to LOG with timestamps (this is what the
orchestrator wants measured):
- All three seats already MIRROR (`read_mirror` says `on: true`): after
  every persist vote applies, every seat records how
  `resolve_upload.local.kind` for the OTHER seats' files moves on its own
  node (none → partial → mirrored) and how many seconds it takes;
  `read_mirror` in between (holders per file, own held/of). Do not touch
  `set_mirror` unless the tool text makes you - then log why.
- Mr. Left downloads one of Center's pictures (`download_file`) BEFORE
  the mirror brought it (right after the persist vote) and records
  `local.kind` before/after, the download's phase/percent/path from
  `read_uploads`, and where the file landed.
- The republic's open proposals from round 2 (a few are still
  `proposed`) are decided first: read each with `read_proposal`, approve
  what is right, decline the rest with a reason - a stale queue is a
  finding, not a blocker.
- Everyone: reference one picture BEFORE its persist vote applied once,
  on purpose, and record what `wiki_edit` said (warning? refusal?) and
  what `wiki_get.files` reads afterwards; then fix it.
- Everyone: one deliberate reference with a 12-hex prefix that names no
  file, in a `dry_run: true` call, to see the warning text; do not
  propose it.
- Everyone: after the run, `wiki_health.files` must read 0/0/0.
- Log every place where the tool text left you guessing (the id format,
  where the exchange folder is, how long a share takes to hash, what an
  `availability` word means, whether a persist vote needs the sharer
  online), every refused call with its exact message, and every detour.

## Flow

0. Kickoff in the group chat (2-3 sentences, in character), then Step 0.
1. Part 1 and Part 2 interleaved: research block → write batch → approve
   loop (approve what is correct, decline what is not, with a reason in
   the patch channel) → sync line → verify. Persist votes are approved
   as soon as they are seen.
2. Rules 4 to 7 continuously.
3. Finale: refresh `uebersicht/<segment>.md` (Right: `zeitachse.md`,
   `vergleich.md`) from facets and neighbours, with at least one picture
   each. `DONE <seat>` in the group chat; then keep approving until all
   three are DONE and `list_proposals` is empty OR a DIVERGENZ was
   reported. There is no time limit; DONE is when your gap list is empty
   and your pictures are in.

## Final report (compact, with numbers)

1. Pages before/after; proposals filed/approved/declined/withdrawn;
   renames; foreign-page sections; disputes and outcomes; messages per
   channel.
2. Pictures: per file name → share id, checksum prefix, persist proposal
   id, seconds from share to persisted, the page(s) referencing it, the
   `files` block `wiki_get` answered, `local.kind` on your node at the
   end. The mirror/download experiment's timeline.
3. `wiki_props` extract; `wiki_health` at the end, `files` included.
4. Sync history: every HEAD line, the first deviation, the reaction.
5. MCP friction log (`friction.md`): every refused call, unclear reply,
   detour, missing tool - what worked, too. Mark what is NEW in this run
   (files/pictures) versus what round 2 already reported.
6. Cooperation: where the others blocked, bypassed, confused or overwrote
   you.
