# Mirroring: every consenting seat keeps the persistent files

**Status: OPEN - design of 2026-09-03, decisions ratified by the user the
same evening (§1), stages M1-M6 not built.** Follows
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

### 3.1 The piece format (series v2)

- At SHARE time the sharer's hashing task (already off the actor,
  `spawn_share_hash`) also draws the 32-byte **content key** `K`, seals
  the file into pieces and computes the manifest:
  `piece_i = AEAD(K_i, nonce 0, plain_i)` with `K_i = HKDF(K, "molt-piece" ‖ le32(i))`
  (one derived key per piece: deterministic, byte-identical re-publishes,
  no nonce bookkeeping), `plain_i` = the i-th 44 000-byte slice, the last
  one zero-padded to the full size (the relay sees uniform blocks);
  `count: u32`.
- **Manifest** = `count ‖ size ‖ sha256(piece_0) ‖ … ‖ sha256(piece_{n-1})`
  over the CIPHERTEXT pieces; **root** = sha256 of the manifest. The chat
  share (`FileMeta`) grows `key_b64`, `pieces: u32` and `root` (additive,
  `#[serde(default)]`; a share without them is a legacy v1 share). The
  persist block copies all three (sign-what-you-see: members ratify the
  identity AND the key). The manifest itself is published as piece
  `count` (the last index + 1), sealed with `K_manifest = HKDF(K, "molt-manifest")`.
- A holder verifies each fetched piece by hash against the manifest and
  the manifest by root; the assembled file by `checksum` (sha256 of the
  plaintext) as today.
- Tags stay the publish stamp's day window (privacy: a fixed per-file tag
  would let a relay group and count a file's pieces for its lifetime).
  A fetcher subscribes the windows from the series' first stamp to now in
  chunks of `MAX_CATCHUP_WINDOWS` (60) - `h_tags_for_catchup` exists.
- v1 series stay fetchable for legacy shares (no key → the exporter path);
  nothing new is published as v1.

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
