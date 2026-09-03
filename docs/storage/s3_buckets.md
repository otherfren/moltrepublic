# Two S3 buckets: workspace backups and media

Status: **BUILT (2026-08-21)** except §7, which stays open BY DECISION: the
media bucket is configuration only until a consumer is designed (a Shared
Files mock surface named it their optional backup on 2026-08-28 and was
removed again on 2026-09-03 — §7 is open as before). Keystones:
`molt-engine/src/backup.rs` (`quota_candidates` unit tests),
`molt-engine/tests/backup_ticker.rs` (the two quota tests),
`molt-engine/tests/s3_test_button.rs` (per-bucket probe + verdict
invalidation), `molt-ui/src/lib.rs::s3_target_tests`.

## 1. What exists today

One S3 target lives in `[storage]` and serves exactly one errand:
**workspace backups**.

| key | meaning |
| --- | --- |
| `s3_backup` | master switch of the backup ticker |
| `s3_endpoint` / `s3_access_key` / `s3_secret_key` / `s3_bucket` | the one target |
| `s3_interval_min` | how often the ticker uploads |
| `s3_keep_copies` | per-workspace retention (count) |

`default_s3_bucket()` is `"media-archive"` - a deliberately **inconspicuous
cover name** (`molt-config/src/lib.rs:332`), not a media bucket. That naming
is what prompted this work; it stays the default for the *backup* bucket.

Nothing in the product writes media to S3. Shared files ride the 445 chunk
plane, and an external blob server was explicitly rejected
(`docs_archive/transport/file_transfer_nostr.md` §1, `mdk_evaluation.md`) -
it adds infrastructure, a second metadata surface and a clearnet dependency
against the onion/self-host posture (ADR-0004).

## 2. Decisions (user, 2026-08-21)

1. **The media bucket is configuration only.** It gets no consumer in this
   change. The GUI says so plainly rather than implying media already lands
   there - CLAUDE.md forbids faking behaviour a user could mistake for real.
   The open consumer question is §7.
2. **A full bucket prunes oldest-first**, it does not refuse the upload.
3. **One endpoint, one access key, one secret - several buckets.**
   (Revised 2026-08-21, superseding the first answer of "own credentials per
   bucket".) The credentials block is configured once; each bucket adds only
   a name and a byte cap. One "Test connection" per bucket stays, because the
   probe is a `HEAD /bucket` and proves *that* bucket, not merely the host.

## 3. Config keys

The existing keys **keep their names**: `s3_endpoint`/`s3_access_key`/
`s3_secret_key` become the shared account, `s3_bucket` stays the backup
bucket. Renaming them to `s3_ws_*` would buy symmetry and cost a migration on
every config.toml in the field plus a broken `save_settings` /
`patch_settings` MCP contract. The asymmetry is paid for in doc comments
instead.

```toml
[storage]
# --- the one S3 account ---
s3_endpoint    = "https://s3.example.org"
s3_access_key  = "…"
s3_secret_key  = "…"

# --- bucket 1: workspace backups ---
s3_backup      = true
s3_bucket      = "media-archive"
s3_max_bytes   = 0          # NEW. 0 = no limit
s3_interval_min = 60
s3_keep_copies  = 5

# --- bucket 2: media (configured, no consumer yet) ---
media_s3_bucket    = ""     # NEW
media_s3_max_bytes = 0      # NEW. 0 = no limit
```

- `*_max_bytes` is `u64`; **0 = no limit**. Note the neighbouring
  `file_cap_bytes` uses 0 for "off" - different sentinel meaning in the same
  table, so both the config comment and the GUI hint say which.
- The media bucket has **no cover-name default**: empty = not configured.
  A second inconspicuous default would just be a name nobody chose.
- All three new keys are `#[serde(default)]`, so an existing config.toml
  parses unchanged under `deny_unknown_fields`.

## 4. Quota semantics (workspace bucket - the real one)

Enforcement runs **after a confirmed upload**, right after the existing
`keep_copies` retention, inside the same off-actor backup task. It reports on
its **own** channel - `NetBackupDone.quota_error`, surfaced as the notice
`backup-quota:…` - beside the count retention's `prune_error`. One notice
slot, so a prune failure (the more basic fault) wins when both spoke; a full
bucket must not read as "pruning old copies failed".

Order of operations per successful backup:

1. `prune_old_copies` (unchanged): per-workspace count retention.
2. **quota pass** (new): list `BACKUP_OBJECT_PREFIX` bucket-wide, keep only
   keys that `parse_backup_key` accepts, sum their sizes.
3. Over `s3_max_bytes`? Delete **oldest first by parsed timestamp** (not by
   key order - keys sort by workspace id first) until the sum fits.
4. Two objects are **never** candidates:
   - the object this backup just uploaded, and
   - the **newest copy of every workspace**. A byte quota must never leave a
     republic with zero backups; that would be silent data loss dressed up
     as retention.
5. If the sum still exceeds the quota once every candidate is gone, say so
   honestly (`quota: <used> B of <limit> B, nothing left to prune`) and
   delete nothing further.

The bucket-wide listing rides `list_objects`, whose 10 000-object cap is a
hard error rather than a silent truncation - a bucket past it reports
`quota: listing failed: …` instead of pruning on half a picture.

**Foreign objects are neither counted nor deleted.** We cannot prune what we
did not write, and counting it would make someone else's upload delete our
backups. The GUI hint states that the limit counts this node's backups.

The decision is a pure function, tested first:

```rust
// QuotaObject = { key, id: WorkspaceId, ts, size }
fn quota_candidates(
    objects: Vec<QuotaObject>,
    max_bytes: u64,
    just_uploaded: &str,
) -> (Vec<String> /*delete, in order*/, u64 /*bytes left after*/)
```

Red-first tests: under quota deletes nothing · `max_bytes == 0` deletes
nothing · over quota deletes oldest first and stops as soon as it fits ·
never the newest per workspace · never `just_uploaded` · unfittable returns
every candidate plus a remainder above the limit.

The media bucket has no writer, so **no quota pass runs for it**. Its
`media_s3_max_bytes` is stored and shown, and that is all it does today.

## 5. Command / session surface

- `SessionView.s3_media_test`: the media target's probe verdict, same
  vocabulary as `s3_test` (`""` / `"testing"` / `"ok"` / `"error: …"`).
- `Command::NetTestS3` gains `target: S3Target` (`workspaces` | `media`,
  `#[serde(default)]` = `workspaces`, so an existing MCP caller keeps
  working). `NetTestS3Result` carries the same discriminator so a verdict
  cannot land in the wrong slot. Both targets probe the shared endpoint and
  credentials; only the bucket differs.
- Editing the shared endpoint or credentials makes **both** verdicts stale
  and clears both. Editing one bucket name clears only that bucket's verdict
  - and the backup bucket additionally drops the backup listing, as today.
- No new `Command` variant, so the co-equality test needs no new entry.

## 6. Surfaces

**MCP** - `save_settings` / `patch_settings` gain the three keys
(`patch_settings` gets them for free: it merges over the serialized
`SessionSettings`). `save_settings` requires every field by contract, so the
new keys are required there too: an existing agent script calling it fails
loudly with the missing key named, rather than silently wiping a bucket or a
quota. `net_test_s3` gains the optional `target`.

**GUI** - the "S3 config" tab (`set-tab == 3`) is one credentials panel
(endpoint, access key, secret) followed by one `S3BucketGroup` per bucket:
name, byte cap, Test button and the streamed verdict. The media panel carries
one short line saying nothing writes there yet. The cap is a **numeric text
field in MiB** (`0` = no limit), not a stepper - a stepper cannot reach a
realistic bucket size - and a stored byte value that still renders as the
same MiB is kept unrounded, so an unrelated save never re-quantizes a
hand-written `s3_max_bytes`. The settings pane is already a Flickable, so
the extra height scrolls.

## 7. Open: what the media bucket is for

Deliberately unanswered. Wiring it up means picking a consumer, and the
obvious one (shared files) reopens the recorded "445 chunking, NOT an
external blob server" decision. That needs its own design pass covering at
minimum: who encrypts the blob and with which key, how the key reaches a
reader, what the object key reveals, and how the Tor posture survives a
second host. Until then the fields are configuration, and the GUI says so.
