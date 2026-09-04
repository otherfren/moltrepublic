# Mirroring: every consenting seat keeps the persistent files

**Status: OPEN - design of 2026-09-03, decisions ratified by the user the
same evening (§1); M1 (the piece format v2, the cap removal) is BUILT in
the change that carries this line, M2-M6 open.** Follows
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
- The fetch subscription carries a stored-event bound sized to the
  series (`SeriesLayout::history_bound` = twice the pieces plus headroom)
  - the relay runtime's default 5 000 would cut a large series short.
- Tags stay the publish stamp's day window (privacy: a fixed per-file tag
  would let a relay group and count a file's pieces for its lifetime).
  The `FileServed` stamp is the series START; a fetcher subscribes every
  window between that stamp and now in EITHER order plus the skew
  neighbours (`file_catchup_tags`, newest `MAX_CATCHUP_WINDOWS` = 60
  kept) - a sharer's clock ahead across a UTC day boundary stamps into
  the fetcher's next window.
- The sharer takes the hour's publish round BEFORE re-reading the file;
  the re-read must still hash to the root (a changed file is refused).
  A relay's "slow down" is waited out per piece (M2 paces instead).
- v1 series stay fetchable for legacy shares (no key → the exporter path,
  its in-memory claim bounded by the configured cap or 1 GiB); nothing
  new is published as v1. The share cap is gone: `file_cap_bytes` absent
  = no cap, 0 = sharing off, n = a deliberate cap (`FileCap`); the old
  unconditional `4194304` in an existing config heals away at boot.

### 3.2 The trickle sender

One queue per node and republic, fed by three sources, drained at a pace:

1. `FileWanted { id }` for an OWN share: enqueue the whole series (v2),
   manifest first. Replaces the lazy whole-series publish.
2. `PieceWanted { id, ranges }` (new control frame, authenticated): a
   holder that has the pieces answers - the sharer if online, else the
   mirrors; to keep N mirrors from re-publishing the same piece, the
   holder with the lowest member name among the CURRENT holders (gossip
   §3.4) answers, the next one after `PIECE_WANT_TIMEOUT` (10 min) if the
   piece is still missing.
3. The mirror worker's own re-seeding is the same path (it holds pieces).

Pace: `mirror_publish_interval_secs` (default 15 → ≤ 240 pieces/h ≈ 10 MB/h
≈ 250 MB/day per node), one event per tick, only while the group outbox
is idle and the hourly resend budget has ≥ 2 rounds of headroom (chat and
governance always come first). Persisted cursor per series (`TransportState`)
so a restart resumes, not restarts. Config keys: the interval and a daily
byte cap (`mirror_daily_bytes`, default 512 MiB); both node-local.

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

### 3.4 Declaration and status (what the members see)

Two control frames, both authenticated by the MLS credential like the
claim sheet:

- `MirrorDecl { on: bool, quota: u64, rev: u64 }` - sent on runtime start,
  on change and every 6 h; stored per member in `TransportState`
  (`mirror_decls`, last-wins by `rev`; `rev` = unix seconds of the
  change). A member without a declaration reads "unknown".
- `MirrorStatus { series: [(id, held: u32, of: u32)] }` - debounced (every
  32 new pieces, every 5 min, and on completion), stored per member in
  memory + with the accept-window saves. "Mirrored by N" = holders with
  `held == of` plus the sharer; the progress bar draws THIS seat's bitmap.

Both are gossip, never chain state; a seat that was offline learns them
from the next periodic send, and can ask (`MirrorWho`, answered by every
holder with its status - once per hour at most).

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
the `.part` file is never pre-sized; the fetch bound follows the series;
a mixed-build republic works - an older build's persist carries no series
material, a new build's approve matches the six identity fields and the
material only when the claim carries it, and such a block pins the share
WITHOUT mirror material (M4 skips it; an unpersist + persist on a new
build restores it); the wire door and the propose door check the
material's shape (`validate_files_payload`); a file that changes between
the root check and a later slice aborts the publish with the round spent
(accepted); the content key rides the chat message and therefore every
read of that message - a member secret, visible to the seat's agent like
the file itself; a relay's rate limit is waited out per piece.
