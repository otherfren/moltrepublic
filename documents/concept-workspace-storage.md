# Concept: creating and persisting workspaces — files and data structures

Status: **implemented through S3** (S0 event-applier refactor, S1
`molt-storage`, S2 create/open/close/delete wiring + id addressing + real
seeds, S3 snapshots). S4–S6 (manual `.molt.enc` export/import, S3 uploader,
passphrase sealing) remain open, as do the fsck tool, golden fixtures, the
VFS fault-injection harness and fuzzing from §8.

Deliberate deviations from the letter of this document:

* **Seed rendering**: the 32-byte CSPRNG root is rendered as 24 BIP-39
  words (Monero-grade 256-bit entropy; Monero itself uses 25-word/256-bit
  seeds) instead of a bespoke wordlist.
* **Unknown newer events**: opening is *refused* with a clear error instead
  of entering read-only mode (read-only gating across the command set is a
  later refinement; refusing can never fork history).
* **Durable-ack / deferred replies** (§6): not yet wired — the writer
  group-commits every 50 ms and a lagging/failed writer surfaces as a
  session notice (`storage-lagging` / `storage-failed`). Relatedly,
  open/close/create still run synchronously on the actor (acceptable at
  current log sizes; move to `spawn_blocking` + deferred reply when logs
  grow), and a full writer queue falls back to a blocking send.
* **Join/restore**: until the network and the restore paths (S4/S5) exist,
  JoinFinish and RestoreFinish materialize their local dir as a *fresh*
  genesis under a fresh seed through the same `molt-storage::create` path
  the founder uses (RestoreFinish is idempotent: an existing "Restored
  Republic" is re-opened, not re-created).
* **Approval dedup**: `apply(Approved)` counts without per-member dedup on
  purpose — the threshold machine is a simulation where one local operator
  stands in for the whole group; real dedup arrives with real member
  identities (FROST/MLS). The envelopes already record `by` for that day.
* **Simulated chat replies** never reach a persisted workspace's log: the
  demo reply bot only runs on session-only workspaces (a canned reply in
  the authoritative encrypted history would replay forever as a real
  message).

## 1. Goal and guiding constraints

* A workspace (= one republic on this device) survives restarts: its
  identity, member roster, and the full history of every surface.
* The storage layer must fit the architecture we are committed to: **the
  engine actor as an event applier** — a command is validated, produces an
  event, and `apply(event)` is the *only* thing that mutates state. Today
  the handlers still mutate directly and emit events afterwards (e.g.
  `cmd_chat` stamps `ts` via `now_secs()` at mutation time); milestone
  **S0** (§9) performs that refactor first, because the replay-determinism
  test (§8) is impossible without it. Persistence is then an **append-only
  event log plus snapshots**, not a mutable database.
* Everything at rest is encrypted (this is a privacy project; the workspace
  dir may sit in a synced folder).
* The wire/data schema follows the codebase rule established with
  `ChatMessage`: **typed structs in `molt-core` are the schema**; files
  serialize those structs.

## 2. Directory layout

```
<storage.workspace_dir>/                      (default ~/.moltrepublic/workspaces)
└── family-office.a1b2c3/                     <slug>.<short-id>: human-readable + unique
    ├── LOCK                                  flock guard, held while open
    ├── manifest.toml                         plaintext identity card (see §3.1)
    ├── prefs.toml                            local node preferences (see §3.1a)
    ├── keys/
    │   └── workspace.key                     0600; sealed workspace key (see §5)
    ├── log/
    │   ├── 000001.mlog                       encrypted, framed event segments
    │   ├── 000002.mlog                       (rotated at ~8 MiB)
    │   └── ...
    ├── snapshots/
    │   ├── 000000000420.msnap                state snapshot at log seq 420
    │   └── ...                               keep newest 2, delete older
    └── tmp/                                  same-fs scratch for atomic renames
```

* **Slug + short id.** `slug(name)` (the create-run already computes it)
  plus 6 hex chars of the workspace id (§5) — display names may repeat,
  even deliberately: the same DAO opened twice locally under two member
  identities is a supported test setup. The id disambiguates, and because
  it is derived from seed *and* member identity (§5), even those two
  instances get distinct ids and directories. The display name lives in
  the manifest, the directory name is never parsed back.
* `tmp/` guarantees rename-atomicity stays on one filesystem.
* The Open screen scans `workspace_dir/*/manifest.toml` only — cheap, no
  decryption needed for the list (see §3.1 for what is deliberately public).
* **One workspace dir belongs to exactly one node.** The `LOCK` flock only
  protects against local double-opens; it cannot protect a directory that a
  file-sync tool (Syncthing, Dropbox, …) replicates to a second machine —
  two nodes appending to the same `.mlog` produce sync conflicts and forked
  history. Encryption-at-rest makes a synced dir safe to *store*, not safe
  to *share*: multi-device is the network protocol's job, never file sync.

## 3. Data structures

All persistent structs live in `molt-core` (schema home), the I/O lives in a
new crate **`molt-storage`** (fits the reserved layering: core holds no
I/O). Every file starts with a format marker and version.

### 3.1 `manifest.toml` — the plaintext identity card

Deliberately *minimal* plaintext: what the Open screen needs before the user
authorizes decryption, and nothing that leaks content.

```toml
format = "molt-workspace"
version = 1

[workspace]
id         = "a1b2c3d4e5f6…"      # 32-byte hex, derived from the seed (§5)
name       = "Family Office"
created    = 1751700000            # unix seconds
rule_m     = 4
rule_n     = 7

[crypto]
kdf        = "argon2id"            # for the key-sealing passphrase path
cipher     = "xchacha20poly1305"
key_file   = "keys/workspace.key"
```

Rust: `struct WorkspaceManifest { format, version, id: WorkspaceId, name,
created, rule_m, rule_n, crypto: CryptoParams }` — parsed with
`deny_unknown_fields` **off** and `#[serde(default)]` where sensible:
manifests written by a *newer* node must stay listable by an older one
(forward compatibility is a feature of the list screen, not of opening).
Opening checks `version <= SUPPORTED` and refuses politely otherwise.

Sync presence data (`last_sync_min`, member last-seen) is **not** stored
here — it is runtime state owned by the transport; the session fills it.
The acting member handle is deliberately not in the plaintext either (it
would leak who you are in the republic); it lives in the encrypted state,
established by the genesis event (§3.2). The manifest is written once at
creation and only rewritten on rename.

**Addressing.** The workspace id is the sole address of a workspace across
the command set: `WorkspaceInfo` gains an `id: WorkspaceId` field,
`OpenWorkspace` / `DeleteWorkspace` / `SetWorkspaceBackup` switch their
`name` parameter to `id`, and `session.active_workspace` becomes an id.
Display names are presentation only and may repeat; the create-flow drops
its local unique-name check once ids land.

### 3.1a `prefs.toml` — local node preferences

Per-workspace settings that are *this node's business*, not shared history:
today that is the auto-backup switch (`SetWorkspaceBackup`) and the last
backup timestamp; later perhaps notification muting. They belong neither in
the manifest (written once, identity only) nor in the event log (not group
state, and toggling a local backup must not fork history). A tiny plaintext
TOML, rewritten atomically via `tmp/` on every change:

```toml
format = "molt-workspace-prefs"
version = 1
s3_backup    = true
last_backup  = 1751700000   # unix seconds, absent = never
```

### 3.2 The event log — one envelope, typed payloads

```rust
/// molt-core
pub struct EventEnvelope {
    /// Strictly monotonic per workspace; the log's primary key.
    pub seq: u64,
    /// Unix seconds (engine clock at apply time).
    pub ts: u64,
    /// Who caused it (member handle for now; MLS leaf identity later).
    pub by: MemberId,
    /// What happened.
    pub body: WorkspaceEvent,
}

pub enum WorkspaceEvent {
    /// seq 1, exactly once: who this republic is. Rule, roster and the
    /// acting member never exist outside the event stream — no state
    /// that never passed through `apply`.
    Founded    { name: String, rule_m: u8, rule_n: u8,
                 member: MemberId, roster: Vec<MemberId> },
    MemberJoined { member: MemberId },       // a seat filled via invite
    Chat(ChatMessage),                       // the existing typed schema
    ChatReacted { index: u64, emoji: String, by: MemberId },
    ChatDeleted { index: u64, by: MemberId },
    Proposed   { id: ProposalId, surface: Surface, payload: Value },
    Approved   { id: ProposalId, by: MemberId },
    Declined   { id: ProposalId, by: MemberId },
    Applied    { id: ProposalId },
    MemberSeen { member: MemberId, ts: u64 }, // roster presence checkpoints
    // additive-only: new variants append; unknown variants on read are
    // preserved as raw frames and re-emitted on compaction (see below)
}
```

The log is therefore never empty: `create` writes the `Founded` genesis as
frame 1, and replay-from-zero reconstructs roster and rule without peeking
at the manifest (the manifest's copies exist only for the undecrypted Open
screen; on open they are cross-checked against the genesis event).

Rule: **additive-only evolution.** New event kinds append enum variants; an
older reader that meets an unknown variant keeps the raw frame (it cannot
apply it, so it must refuse to *write* to that workspace — read-only mode —
because applying a partial history would fork state). `version` in the
manifest gates this: writers bump it when introducing variants readers must
understand.

Implementation note: serde fails the whole envelope on an unknown enum
variant, so decoding is two-stage — try `EventEnvelope`, on failure fall
back to a `RawEnvelope { seq, ts, by, body: Value }` that preserves the
frame for re-emission. A dedicated round-trip test pins this behavior.

### 3.3 On-disk framing (`.mlog` segments)

Content is a sequence of frames; the whole segment is encrypted per frame so
appends never rewrite:

```
frame     := len:u32le | crc32c(ciphertext):u32le | nonce:24B | ciphertext
plaintext := serde_json(EventEnvelope)      # JSON: debuggable via export tool
aad       := workspace_id | segment_no:u64le | seq:u64le
```

* **CRC over the ciphertext, never the plaintext.** The crc exists solely
  for torn-write/bitrot detection without decrypting; plaintext integrity
  is the Poly1305 tag's job. A plaintext crc in a cleartext header would
  hand an attacker a confirmation oracle for guessed content — exactly the
  leak the encrypted-dir-in-a-synced-folder threat model forbids.
* **AAD binds position.** Each frame is authenticated against
  `(workspace_id, segment number, seq)` as associated data: an intact frame
  cannot be reordered, replayed, or transplanted into another segment or
  workspace without the AEAD open failing.
* **Torn-write recovery**: on open, scan the last segment; a frame whose
  `len`/`crc` does not check out marks the torn tail — truncate to the last
  valid frame boundary. Everything before it is intact (append-only).
* **fsync policy**: group commit — fsync at most every 50 ms *or* before
  acking a command whose loss the user would notice (send chat: yes;
  presence checkpoint: batched). One knob, measured, documented.
* **Rotation** at ~8 MiB keeps recovery scans and S3 diff-uploads bounded.
* JSON now, with the frame header making the codec swappable (a `codec` byte
  in the segment header allows CBOR later without migration drama).

### 3.4 Snapshots

`WorkspaceSnapshot { version, at_seq, state: EngineStateDump }` where
`EngineStateDump` is exactly what the actor holds today: `chat:
Vec<ChatMessage>`, per-surface applied logs, proposals map, roster. Written
by a background task (see §6) every N events (default 1 000) or on clean
close; loading = newest valid snapshot + replay of frames `> at_seq`.
Snapshots are an *optimization* — deleting them must always be safe, and the
test suite proves replay-from-zero equals snapshot-plus-tail.

Compaction (optional, later): rewrite old segments dropping events whose
effect is fully captured by a snapshot floor — only once a retention story
is decided; the append-only default is to keep everything (it *is* the
shared history of the republic).

## 4. Lifecycle wiring (what replaces which mock)

| Flow | Today | With storage |
|---|---|---|
| CreateFinish | pushes `WorkspaceInfo` into the session | `molt-storage::create(dir, manifest, seed)` → workspace dir born atomically (`tmp/new-…` + rename), key sealed, log opened with the `Founded` genesis frame; then session entry as now |
| JoinFinish | pushes `WorkspaceInfo` into the session | same `molt-storage::create` path — the joiner materializes its local dir with the genesis + history received from the group |
| OpenWorkspace | flips session fields | acquire LOCK, load snapshot+tail into the actor, then as now |
| CloseWorkspace | flips session fields | flush + fsync, write closing snapshot, release LOCK |
| DeleteWorkspace | removes list entry | move dir to `workspace_dir/.trash/<slug>-<ts>` (recoverable), delete `.trash` entries older than 30 days at startup |
| Manual backup (`.molt.enc`) | display note | `tar` the dir (excluding LOCK/tmp) through the workspace cipher into the chosen path — the restore-from-file run consumes exactly this |
| S3 backup | display note | background uploader task ships closed segments + newest snapshot (they are already encrypted; the bucket learns sizes and timing only) |
| Restore runs | simulated log lines | the real implementations of the three ways, writing through `molt-storage::create` |
| SetWorkspaceBackup | flips a session flag | additionally persisted to the workspace's `prefs.toml` (§3.1a) |

One deliberate shape change: the workspace-addressing commands switch from
display name to workspace id (§3.1 “Addressing”). Everything else keeps its
existing command/tool surface — the co-equality catalogue otherwise does not
change shape, the mocks just gain organs.

## 5. Keys and encryption

* **The recovery seed is the root of the entire key hierarchy.** It is the
  only true randomness in a workspace — 32 B from the OS CSPRNG at creation,
  rendered as the recovery phrase (`mock_seed`/its LCG wordlist must not
  survive into this path: a key hierarchy hanging off ~30 bits of hashed
  wall-clock is decorative encryption). Every identifier and key derives
  from the seed via HKDF, directly or indirectly; restore therefore always
  needs the seed — the decryption key comes from nowhere else.
  * `workspace_id  = HKDF(seed, "molt-ws-id", member_handle)` —
    deterministic, so seed + own handle re-derive the identity; including
    the member handle is what gives two local instances of the same
    republic (same seed, two identities) distinct ids and directories.
  * `workspace_key = HKDF(seed, "molt-ws-key", workspace_id)` (32 B), used
    with XChaCha20-Poly1305 for frames, snapshots and exports.
  * Restore-from-file/S3 additionally has the plaintext manifest and uses
    its `id` as a cross-check against the re-derived one.
* The key is stored **sealed** in `keys/workspace.key` (so day-to-day opens
  never touch the seed):
  * v1: sealed to a device key held in `~/.moltrepublic/device.key` (0600) —
    no passphrase prompt, honest threat model note: protects the synced/
    backed-up workspace dir, not a fully compromised home directory;
  * v2 (opt-in): passphrase sealing via argon2id — the parameters already
    have a home in the manifest's `[crypto]` table.
* Nonces are random per frame (24 B XChaCha nonce space makes counter
  bookkeeping unnecessary); key rotation = new segment generation tag in the
  segment header (design slot, not v1).

## 6. Concurrency & parallelism

* **One writer task per open workspace** (`StorageHandle` with an mpsc
  queue), same pattern as the engine actor and the ConfigStore: the engine
  applies an event in memory, emits it as today, and enqueues the envelope;
  the storage task frames, encrypts, appends, and group-commits. The actor
  never blocks on disk.
* **Backpressure**: the queue is bounded (e.g. 1 024). If storage falls
  behind (dying disk), the engine notices the full queue and degrades
  explicitly: `session.notice = "storage-lagging"`, and commands that must
  be durable (chat send) start awaiting the fsync acknowledgment instead of
  fire-and-forget. Silent data loss is not an option; slow honesty is.
  * *How awaiting works without freezing the actor*: `State::handle` is
    synchronous and single-threaded — it must never block on disk. The
    durable-ack path instead **defers the reply**: the handler applies the
    event, hands the `Envelope`'s `oneshot` reply sender to the storage
    task alongside the frame, and returns without answering; storage fires
    the `Ack` after its fsync. Only the one waiting operator is delayed —
    the actor keeps processing everyone else's commands.
* **Snapshot task**: spawned by the storage task; works from the in-memory
  dump handed over at trigger time (no log locking — the log is append-only
  and the snapshot names its `at_seq`).
* **Open-screen scanning** reads manifests on `spawn_blocking`, parallel per
  directory entry (`join_all`), and feeds the session in one command.
* **Exports/S3** run on snapshot + closed segments only — never touch the
  active segment, therefore never contend with the writer.
* Cross-process: `LOCK` (flock, exclusive) per workspace; a second node or a
  second open attempt gets `MoltError::WorkspaceBusy` with the holder's PID.

## 7. Failure matrix

| Failure | Behavior |
|---|---|
| crash mid-append | torn tail truncated at open; at most the unacked suffix is lost (bounded by the fsync policy) |
| crash mid-snapshot | snapshot written to `tmp/` + rename; a torn snapshot never shadows an older valid one |
| disk full | storage task reports; engine surfaces `storage-full` notice; chat sends fail loudly (durable-ack path) |
| corrupted middle frame (bitrot) | crc catches it; open fails with the segment/frame position; `--fsck-workspace <dir>` tool truncates or quarantines with explicit consent |
| unknown newer event variant | workspace opens read-only with a clear notice |
| frame replayed / moved between segments or workspaces | AEAD open fails — the AAD binds `(workspace_id, segment, seq)` (§3.3) |
| deleted while open elsewhere | flock prevents it locally; external deletion surfaces on next append as an I/O error, workspace closes with notice |

## 8. Testing

* **Round-trip**: create → write scripted event sequence → close → open →
  actor state equals a pure in-memory run of the same commands (determinism
  is what S0 establishes; this is the keystone test).
* **Property tests**: arbitrary event sequences — `replay(log) ==
  replay(snapshot at k) + replay(tail)` for every k; framing round-trip;
  truncate-anywhere recovery (chop the file at every byte offset of the
  last frame → open never panics, recovers the maximal valid prefix).
* **Fault injection**: a `VfsTrait` (std-fs impl + failing impl) lets tests
  inject ENOSPC/EIO at each syscall boundary; assert the failure matrix.
* **Fuzzing**: `cargo fuzz` target on the frame decoder (untrusted input:
  restored/imported files).
* **Concurrency**: two open attempts → second gets `WorkspaceBusy`;
  writer-lag test with a slow VFS asserts the backpressure notice and the
  durable-ack switch; snapshot-during-burst produces a consistent `at_seq`.
* **Golden fixtures**: one committed workspace dir per `version` (tiny,
  generated) — opening old fixtures stays green forever, which is the
  migration contract.
* **E2E**: existing MCP harness — found a republic, send chat, restart
  `moltd`, `read_state` shows the history; manual backup file feeds the
  restore-from-file run and produces an identical workspace.

## 9. Milestones

1. **S0** engine refactor to a true event applier: command → validate →
   `WorkspaceEvent` → `apply(EventEnvelope)` as the *only* state mutator;
   timestamps and identities come from the envelope (clocks run only at
   event creation, `now_secs()` leaves the handlers). Pure in-memory, no
   storage — but the §8 keystone test becomes formulable and CI-locks the
   engine's determinism before any byte hits disk.
2. **S1** `molt-storage` crate: framing, crc, encryption + AAD, torn-tail
   recovery, round-trip + truncate-anywhere tests. No engine wiring.
3. **S2** create/open/close/delete wired (chat + proposals persist); LOCK;
   Open screen scans manifests; real seed entropy (OS CSPRNG + real
   wordlist) replaces `mock_seed` for founded workspaces; `WorkspaceInfo`
   gains its `id` and the addressing commands switch from name to id.
   *(the “it survives a restart” demo)*
4. **S3** snapshots + startup replay budget; fsck tool.
5. **S4** manual `.molt.enc` export/import = the real restore-from-file.
6. **S5** S3 uploader task; restore-from-S3 real.
7. **S6** passphrase key sealing (opt-in).
