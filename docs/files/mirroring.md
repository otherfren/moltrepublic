# Mirroring: every consenting seat keeps the persistent files

**Status: OPEN - design of 2026-09-03, decisions ratified by the user the
same evening (§1); M1 (the piece format v2, the cap removal), M2 (the
trickle sender, the resumable fetch, `PieceWanted`) and M3 (the
declaration and status gossip, `set_mirror`/`read_mirror`) are BUILT,
M4-M6 open.** Follows
`docs_archive/files/persistent_uploads.md` (the persist/unpersist votes,
landed the same day).

## 1. Decisions (user, 2026-09-03)

1. **Relays are the only piece path**, like all other traffic - at LOW
   priority: small pieces, never a burst, and if a large file takes days
   that is fine. Privacy over speed.
2. **Per-file size is unlimited.** The 1 GB default quota is the TOTAL
   mirror budget of a seat per republic; at the quota the client stops and
   warns.
3. **One random content key per file**, carried in the persist block
   (members only, MLS); pieces = AEAD(key, index); the mirror folder holds
   ciphertext only.
4. **The sharer computes the Merkle root** over the piece hashes at share
   time; the piece-hash list is fetchable as piece 0; the persist block
   carries the root; every holder verifies every piece against it. Anyone
   may propose persist.
5. Consent and quota are per republic; the folder is private
   (`prefs.toml`). The declaration (on/off, quota) and each seat's mirror
   status are visible to every member ("who mirrors what").
6. Unpersist: mirrors keep the pieces until the fresh window ends, then
   delete them.
7. Default folder `<workspace_dir>/../mirror/<republic-id>/`.
8. Order: the vote core first (done), mirroring as its own stage, every
   stage green on master.

Two consequences the user did not spell out but that follow from 1+2:
the 4 MiB share cap (`file_cap_bytes`, FP4) goes - the key stays as the
"0 = sharing off" switch; and the lazy full-series publish
(`file_transfer_nostr.md` §5.2) is replaced by the same trickle sender the
mirroring needs, because a 1 GB series is 24 000 events, not "one round".

**One deviation from the answer to fork 5, argued here:** the declaration
does NOT become a chain block. The chain has no single-signer block kind
(Membership blocks are threshold-signed and exist only at join/recovery;
`member_relays` is a side field of those), and adding one touches
`verify_chain`, the checkpoint preimage (a v9 with a new ledger) and every
recompute site for a fact nobody votes on. The declaration is a
self-signed, MLS-authenticated control frame instead, persisted per member
at every holder (last-wins by revision) and re-sent on start, on change
and every 6 h - the same visibility, none of the machinery. The user can
veto this in review.

## 2. What exists

- `file_transfer_nostr.md` (F1-F4): a share's bytes ride kind-447 events,
  44 000 B plaintext per event (`FILE_CHUNK_PLAIN_LEN`), sealed with the
  epoch's EXPORTER secret (8-deep ring - epoch-bound, unusable for
  "forever"), under the publish stamp's day-window `h` tag, `count: u16`.
  Published lazily and whole on the first `FileWanted`; fetched by
  subscribing that stamp's window (`fetch_series`), verified against the
  share's sha256. One series consumes one of the 12 hourly publish rounds
  the resends share (`consume_resend_round`).
- The engine side (`net/files.rs`, `transfer.rs`): `FileWanted` /
  `FileServed { at }` announce, `spawn_series_publish`, `spawn_nostr_fetch`,
  the stale-stamp re-publish loop, `files.series` (the announced stamps),
  `share_paths` (the sharer's local path), `prefs.shared_files`.
- Control frames: `CONTROL_FRAMES` in `supervisor.rs` (NUL-tagged MLS
  plaintext, authenticated by the credential - the claim sheet and the
  poke are the templates). The relay pool and `TransportState` persist per
  workspace through `StateStore::update`.
- `persistent_uploads.md`: `files_state()`, `share_identity()`,
  `share_expiry()`, the persist block with the file identity.

## 3. Design

### 3.1 The piece format (series v2) - as built in M1

- At SHARE time the sharer's hashing task (`spawn_share_hash`, off the
  actor) draws the 32-byte **content key** `K` and, in the SAME streaming
  pass as the checksum, the manifest: the sha256 of every UNPADDED
  plaintext slice (44 000 bytes each, `PIECE_PAYLOAD_LEN`). The chat share
  (`FileMeta`) grows `key_b64`, `pieces` and `root` (additive, skipped
  when empty - a share without them is a legacy v1 share); the persist
  block copies all three (sign-what-you-see: members ratify the identity
  AND the key).
- A piece is a kind-447 event exactly like a v1 chunk, with ONE outer AEAD
  layer keyed by `outer_key(K) = HKDF-SHA256(K, "molt-piece-outer-v2")`
  instead of the epoch exporter (the same `seal_outer`: a random 12-byte
  nonce per publish, so re-publishes differ on the wire and holders dedup
  by index after opening). Plaintext = `index u32le ‖ count u32le ‖ len
  u32le ‖ payload`, zero-padded to the uniform block. The key IS the
  series filter: another file's or group's piece fails the AEAD.
- The manifest has TWO wire levels above the data: the slice-hash list,
  chunked at indices `count..count+k` (1 375 hashes per chunk), and ONE
  top record at `count+k` = `count u32le ‖ size u64le ‖ sha256(chunk_0) ‖
  …`; **root** = sha256(top record). A holder verifies the record by
  root, each chunk by the record, each slice by its chunk - a forged
  piece of ANY level is dropped by hash, never a hard error (`Manifest`,
  `TopRecord`, `SeriesLayout`). The record must fit one piece, which
  bounds a series at `MAX_SERIES_PIECES` (~83 GB).
- Relays replay stored events NEWEST first, so the manifest (published
  first, hence oldest) tends to arrive LAST: a fetch lands every slice as
  it arrives (AEAD-authenticated under the members' key), remembers its
  hash, and verifies it once its chunk is known; a slice that then fails
  is re-marked missing and the honest one overwrites it. Nothing waits
  for the manifest, nothing is dropped for lack of it. The `.part` file
  is never pre-sized (a hostile size claim allocates nothing); the
  landed file must hash to the share checksum before the rename.
- The fetch subscription has NO stored-event cut-off: its REQ replays
  every file's pieces under the day's tags, so no per-series number is
  right; a 32-event fan-in channel backpressures a fast relay instead.
  The QUIET window (10 s without an event) ends an honest fetch; a
  ceiling of an hour (`FETCH_CEILING`) stops a hostile trickle - and a
  retry replays the foreign pieces again, which M2's resumable job makes
  moot.
- Tags stay the publish stamp's day window (privacy: a fixed per-file tag
  would let a relay group and count a file's pieces for its lifetime).
  The `FileServed` stamp is the series START; a fetcher subscribes every
  window between that stamp and now in EITHER order plus the skew
  neighbours (`file_catchup_tags`, newest `MAX_CATCHUP_WINDOWS` = 60
  kept) - a sharer's clock ahead across a UTC day boundary stamps into
  the fetcher's next window.
- The sharer takes the hour's publish round BEFORE re-reading the file;
  the re-read must still hash to the root (a changed file is refused).
  A piece's publish retries every transient refusal (a relay's "slow
  down", a timeout, the pool's own breaker) for two minutes
  (`PUBLISH_PIECE_PATIENCE`, `min(250 ms × 2^n, 10 s)` between attempts)
  and ends the series at once on a permanent one (a relay's verdict, an
  empty or gated pool, a local Framing/Crypto refusal).
- v1 series stay fetchable for legacy shares (no key → the exporter path,
  its in-memory claim bounded by the configured cap, with sharing off by
  the old 4 MiB default, else by 1 GiB); nothing new is published as v1.
  The share cap is gone: `file_cap_bytes` absent = no cap, 0 = sharing
  off, n = a deliberate cap (`FileCap`); the old unconditional `4194304`
  (`LEGACY_FILE_CAP_BYTES`) in an existing config READS as no cap at load
  - the file is never rewritten for it, so an older build still opens it
  - and can never be stored as a cap (the settings door refuses it).

### 3.2 The trickle sender - as built in M2

ONE queue of publish jobs per runtime (`TransportState.file_jobs.publish`,
additive, persisted through the storage writer like the cursors), one
entry per series: `{series, key, path, count, size, root, ranges, next,
started_at}` - the pieces to publish as inclusive index ranges in publish
order, `next` the position within them (a restart RESUMES at `next`,
never restarts). Fed by two sources:

1. `FileWanted { id }` for an OWN v2 share: the sharer announces
   `FileServed { at: started_at }` at once - BEFORE the first piece - and
   queues the whole series, top record first, then the manifest chunks,
   then the data (`whole_series_ranges`). Every want is answered with the
   announcement (a requester that lost the stamp must hear it again); a
   series already queued is not queued twice.
2. `PieceWanted { id, ranges }` (`\x00molt-pwant-v1`, authenticated by
   the MLS credential like the poke, ≤ 64 ranges): in M2 ONLY the sharer
   answers, with exactly the ranges asked, bounded to the series' layout;
   holder election is M4. A range job merges into a queued range job; a
   whole-series job absorbs it.

Pace (`molt_net::trickle`): one kind-447 event per
`mirror_publish_interval_secs` (default 15, at least 1), and only while
the group outbox is idle (`GroupHandle::outbox_busy`, raised for a pass
with own frames), the hour's resend budget keeps ≥ 2 rounds of headroom
(`resend_headroom` - chat and governance first) and the UTC day's byte
counter (`file_jobs.sent_day/sent_bytes`, `PIECE_WIRE_BYTES` = 58 720 per
piece) stays under `mirror_daily_bytes` (default 512 MiB). Each piece is
stamped at its own publish time. The manifest is rebuilt from the file
ONCE per process and series and cached; every data slice is re-read at
its offset and checked against it - a changed file drops the job. A
transient refusal backs off (interval × 2^n, cap 64×); a permanent one
too, so a relay's verdict repeats at most every 64 intervals. Both keys
live in `[files]` of `config.toml`, in `SessionSettings`, and on the
`save_settings`/`patch_settings` doors (optional, defaults); they are
read when the runtime is built - a change applies at the next workspace
open.

The v1 whole-series publish and its hourly round stay for legacy shares
only; nothing new spends a round.

The requester's fetch is a persisted job too (`file_jobs.fetch`:
identity, destination, `started_at`, the verified-piece bitmap). It runs
`fetch_series_v2_with` with NO ceiling: the subscription stays open (the
catch-up windows from `started_at` plus the live window, re-placed at the
day roll), every verified data piece marks the bitmap, which persists at
most a second behind (`save_fetch_job`, a synchronous storage message
that stays FIFO with the clean-close merge - a close loses at most that
second, and the relay replays it). At open the engine resumes every
unfinished job at its bitmap (`resume_file_jobs`; the `.part` is kept
and only the missing indices land) and seeds the series stamps from the
publish jobs. After each quiet slice (10 s without a piece) the job
computes its missing ranges; once `PIECE_WANT_AFTER` (10 min) has passed
since the job started it sends `PieceWanted` for them, and again every
30 min while incomplete. Progress rides `DownloadView` (percent =
verified pieces). The job ends on completion, on a failure (both remove
it) - the honest failures left are a landing that cannot be written and
a relay pool that cannot be dialed at start.

### 3.3 The mirror worker

Per republic, on the 1 s delivery tick (cheap checks) and a 60 s planning
tick:

- **Eligibility**: consent on, and `stored + next_file_size ≤ quota`.
- **Choice**: among persistent files (`files_state()`) this seat does not
  fully hold, the one with the FEWEST known complete mirrors (§3.4), ties
  by the older persist; the sharer counts as a holder.
- **Fetch**: subscribe the series' windows (catch-up, 447), store each
  verified piece as `<mirror_dir>/<republic-id>/<series-hex>/<index>` (one
  file per piece - simple, resumable, no sparse-file semantics), keep a
  bitmap in `TransportState` (`mirror_state[series] = held bitmap +
  bytes`). Pull pacing: at most `mirror_fetch_interval_secs` (default 5)
  between pieces - Tor and the relays, not the disk, are the bottleneck.
- **Missing pieces** after the windows drained: `PieceWanted` for the
  missing ranges, then wait; retry every 30 min while incomplete.
- **Quota reached**: stop, ONE notice (`mirror: quota reached - X of Y GB`)
  and a banner above the Persistent table with the same line; no per-file
  repetition. Raising the quota or an unpersist resumes the worker.
- **Unpersist**: when `share_expired(id)` of an unpersisted share becomes
  true, delete its pieces and bitmap (the sharer's own file is never
  touched - C4).
- **Own shares** are never mirrored (the file IS on this disk); they count
  as held for the status.

### 3.4 Declaration and status - as built in M3

Three control frames (`molt_net::mirror_gossip`), each its own tag and
version boundary, authenticated by the MLS credential like the poke, and
`by` checked against it:

- `MirrorDecl { on, quota, rev }` (`\x00molt-mdecl-v1`) - sent at runtime
  start, on `set_mirror`, and every 6 h; stored per member in
  `TransportState.mirror.decls`, last-wins by `rev` (unix seconds of the
  change; a lower revision never overwrites). A member without one reads
  `known: false`.
- `MirrorStatus { holds: [(id, held, of)] }` (`\x00molt-mstat-v1`) - what
  the seat holds; in M3 its own available v2 shares, whole (M4 adds the
  mirrored series). Sent when the list changed (at most once a minute)
  and unchanged every 5 min while non-empty; stored per member in
  `TransportState.mirror.status`, replacing the last. "Mirrored by N" =
  the members whose status says `held == of` plus the sharer.
- `MirrorWho` (`\x00molt-mwho-v1`) - sent ONCE per runtime start; every
  holder answers with its status, at most once an hour.

All three are gossip, never chain state; a seat that was offline learns
them from the next periodic send or its own ask at start. The beat rides
the 30 s presence tick; this seat's own switch, quota and revision live
in `TransportState.mirror` too (default on, 1 GiB), carried by the
storage writer's transport merge like the cursors.

`set_mirror { on, quota_bytes }` and `read_mirror` are tools on both
surfaces (co-equal): the view lists this seat's switch and quota, every
roster member's declaration (`known`, `on`, `quota`, `rev`) and per share
its holders plus this seat's `held/of`; `read_uploads` rows carry
`mirrors`, `mirror_held`, `mirror_of` as well.

### 3.5 Storage and settings

- `WorkspacePrefs` (private, `prefs.toml`): `mirror_dir` (default
  `<workspace_dir>/../mirror/<republic-id>/`). Node-private, any-path →
  the setting is GUI/config-only (`mcp-security.md`: the agent operates
  the seat, not the machine).
- `TransportState`: `mirror_on: bool` (default true), `mirror_quota: u64`
  (default 1 GiB), `mirror_state`, `mirror_decls`, the sender cursors.
  On/quota are settable over MCP (`set_mirror {on, quota_bytes}`) and the
  GUI - co-equal.
- Cap removal: `file_cap_bytes` keeps only its 0 = off meaning; the
  over-cap refusals in `cmd_share_file`, `publish_series` and
  `serve_file_wanted` go, `FILE_CAP_DEFAULT_BYTES` with them.

### 3.6 The GUI

Above the Persistent table: the consent switch ("Mirror persistent
files"), the quota (GB field), the folder (picker; private) and the usage
line ("2.1 of 1.0 GB" turns the warning colour at the quota). Columns:
"Mirrored by" (N) and, while this seat's copy is incomplete, a segmented
bar (up to 64 segments, each the fill of its share of the bitmap). The
Temporary table is untouched.

### 3.7 MCP

`read_uploads` rows gain `mirrors: u32`, `mirror_held/mirror_of` (this
seat), `read_state files` unchanged; `set_mirror` as above; the
declaration and status frames are INTERNAL (transport tasks speaking).

## 4. Stages (each red-first, each green on master)

- **M1** piece format v2 + key/root at share time, u32 count, manifest as
  the last piece, cap removal; keystones over `MockRelay`
  (`molt-net/tests/file_plane.rs`): seal/verify round trip, a tampered
  piece refused by hash, a v1 share still fetches.
- **M2** the trickle sender: replaces the lazy publish; `PieceWanted` +
  lowest-holder election; persisted cursors; keystone: a 3-piece series
  drains one piece per tick and resumes after a restart.
- **M3** declarations + status gossip, persistence, the `MirrorWho` ask;
  keystone: two nodes see each other's declaration and completion.
- **M4** the mirror worker: eligibility, least-mirrored choice, piece
  store + bitmap, `PieceWanted` for gaps, quota stop + ONE notice,
  unpersist deletion; keystone over relays (`file_over_relays.rs` twin):
  a persisted share is complete on a second seat without the sharer
  serving it directly, a quota of one piece stops with the notice.
- **M5** GUI + MCP (§3.6, §3.7), headless keystones for the switch, the
  banner and the bar.
- **M6** docs: this file and `persistent_uploads.md` to the archive as
  executed, `file_transfer_nostr.md` §5.1/§5.2 corrected (cap, lazy
  publish), `s3_buckets.md` §7 closed as "not needed".

## 5. Defaults chosen here (not asked)

Publish interval 15 s and daily cap 512 MiB; fetch interval 5 s; day-window
tags kept for privacy; the lowest-name holder re-seeds; catch-up 60
windows per subscription; one file per piece on disk; status debounce 32
pieces / 5 min. Each is a named constant or config key, none a design
commitment.

M1 decisions (2026-09-03, worker): ONE outer layer keyed by the derived
file key instead of a per-piece HKDF key with a fixed nonce - the existing
`seal_outer` does it, the random nonce costs nothing, and holders dedup by
index anyway; the manifest hashes the plaintext slices (a holder with the
key verifies what it lands, and the plaintext hash also lets a re-publish
under a fresh nonce verify); the manifest is chunked at indices `count..`
(it outgrows one block past ~1 375 pieces); the whole-series M1 publish
still costs one hourly round (M2 changes the pacing).

M1 review round (2026-09-03): the manifest became two levels (chunks +
top record) so a forged chunk is dropped by hash like a forged slice;
slices land as they arrive and verify later (relays replay newest first);
the `.part` file is never pre-sized; the wire door and the propose door check the
material's shape (`validate_files_payload`); a file that changes between
the root check and a later slice aborts the publish with the round spent
(accepted); the content key rides the chat message and therefore every
read of that message - a member secret, visible to the seat's agent like
the file itself; a relay's rate limit is waited out per piece.

M1 second review round (2026-09-04): a FILE subscription has no
stored-event cut-off at all - its REQ replays every file's pieces under
the day's tag, so no per-series number is right - and a 32-event fan-in
channel instead, so a fast relay's replay parks on `send` and the disk,
not RAM, holds the series; a piece publish retries every transient
refusal (20 attempts, 250 ms × attempt, cap 2 s) and only a local
Framing/Crypto refusal ends the series; a data slice must have the length
its index implies before it may land, a landed slice is never overwritten
(a differing candidate waits in a side buffer, at most 4 per index and
32 MiB, until its chunk decides), and the landed file is cut to the top
record's size before it is hashed; manifest-chunk candidates spill to
`<part>.mspill` (64 per slot, 512 MiB; 64 MiB in memory for a path-less
sink) so a forger cannot evict the honest chunk by volume; the approve
gate matches the WHOLE identity, series material included - an older
build's persist for a v2 share is refused at every current approve door
("the proposal lacks the series material - propose it from a current
build"), a legacy share needs none and may not be given any; with sharing
OFF a legacy v1 fetch keeps the old 4 MiB bound.

M1 third review round (2026-09-04): no config marker and no boot-time
rewrite (a rewrite would break rollback to an older build - `NodeConfig`
denies unknown fields - and touched a file holding secrets without a
backup): the old default reads as no cap at load, the file stays, the
settings door refuses that exact value. The publish retry is time-bounded
and tells a relay's verdict from a transient refusal
(`transient_publish_error`). The fetch ceiling is an hour, the quiet
window the honest end. Mixed builds, honestly: an old-build QUORUM can
still seal a material-less persist for a v2 share (the approve check is
per seat, the chain verifies signatures only); the fold therefore KEEPS
the material of any earlier block for that share, and a stale
material-less vote does not block a current build's re-proposal
(`open_files_vote`). The manifest spill file is `<part>.mspill` literally,
removed on every fetch exit, on a retry's sink open, and swept by age
(a day) when a landing is prepared in that directory.

M2 decisions (2026-09-04, worker): the manifest is cached in memory per
process and series instead of persisted (one disk pass per process, not
per want; a restart pays one more); the job persists its source PATH in
the encrypted `transport.state` (node-local like `prefs.shared_files`);
the two pacing keys are written to `config.toml` only off their defaults,
so a config an older build still opens stays that way; the storage
writer's `SaveTransport` merge now carries `file_jobs` beside the cursors
(it silently dropped every other field - the first keystone found it);
the fetch bitmap persists through a synchronous storage message, because
the clean-close path seals `transport.state` before the aborted fetch's
drop could save (a post-merge save is ignored by design); the v2 fetch
builds its channel WITHOUT the exporter ring (`nostr_file_channel`) - at
reopen the ring-bearing context was refused before the group had settled;
`PIECE_WANT_AFTER` is a static with a hidden test seam
(`__set_piece_want_after`), like the reopen transport seam; there is no
cancel command yet, so "the user cancels" ends a job only by a workspace
close (the job resumes at the next open). Keystones:
`molt-net/tests/trickle.rs` (order and pace, resume at the cursor, the
outbox and budget gates, the daily cap over a clock seam, an exact range,
queue dedup) and `molt-engine/tests/file_trickle.rs` (a five-piece
download at one piece per second, a requester closed and reopened
mid-way resuming at its bitmap, a piece the relay lost recovered through
`PieceWanted` against a relay whose database the test prunes).

M3 decisions (2026-09-04, worker): the status persists in
`transport.state` with the declarations (not only in memory) so a reopen
reads "who mirrors what" before the first frame arrives; the ask goes out
once per runtime start rather than on demand (the answer is
rate-limited to one per holder and hour); a status frame carries at most
4 096 series and 256 KiB; the own hold list is sorted by id so an
unchanged set never re-sends. Keystone: `molt-engine/tests/mirror_gossip.rs`
- a declaration reaches the peer, a shared file shows its sharer as the
whole holder in `read_mirror` and in both seats' upload rows, and both
survive the peer's close and reopen.
