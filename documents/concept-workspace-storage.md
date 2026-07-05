# Concept: creating and persisting workspaces — files and data structures

Status: **design**. Today every workspace lives only in the engine session
(`WorkspaceInfo` mock list); founding/joining/restoring produce in-memory
entries and the “manual backup” is a display note. This document specifies
the on-disk reality those flows will write.

## 1. Goal and guiding constraints

* A workspace (= one republic on this device) survives restarts: its
  identity, member roster, and the full history of every surface.
* The storage layer must fit the architecture we already have: **the engine
  actor is an event applier** — commands mutate state, events describe what
  happened. Persistence is therefore an **append-only event log plus
  snapshots**, not a mutable database.
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
  plus 6 hex chars of the workspace id — two republics may share a display
  name; the id disambiguates. The display name lives in the manifest, the
  directory name is never parsed back.
* `tmp/` guarantees rename-atomicity stays on one filesystem.
* The Open screen scans `workspace_dir/*/manifest.toml` only — cheap, no
  decryption needed for the list (see §3.1 for what is deliberately public).

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
id         = "a1b2c3d4e5f6…"      # 32-byte hex, random at creation
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
The manifest is written once at creation and only rewritten on rename.

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

Rule: **additive-only evolution.** New event kinds append enum variants; an
older reader that meets an unknown variant keeps the raw frame (it cannot
apply it, so it must refuse to *write* to that workspace — read-only mode —
because applying a partial history would fork state). `version` in the
manifest gates this: writers bump it when introducing variants readers must
understand.

### 3.3 On-disk framing (`.mlog` segments)

Content is a sequence of frames; the whole segment is encrypted per frame so
appends never rewrite:

```
frame := len:u32le | crc32c(plaintext):u32le | nonce:24B | ciphertext
plaintext := serde_json(EventEnvelope)      # JSON: debuggable via export tool
```

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
| CreateFinish | pushes `WorkspaceInfo` into the session | `molt-storage::create(dir, manifest, seed)` → workspace dir born atomically (`tmp/new-…` + rename), key sealed, empty log; then session entry as now |
| OpenWorkspace | flips session fields | acquire LOCK, load snapshot+tail into the actor, then as now |
| CloseWorkspace | flips session fields | flush + fsync, write closing snapshot, release LOCK |
| DeleteWorkspace | removes list entry | move dir to `workspace_dir/.trash/<slug>-<ts>` (recoverable), delete `.trash` entries older than 30 days at startup |
| Manual backup (`.molt.enc`) | display note | `tar` the dir (excluding LOCK/tmp) through the workspace cipher into the chosen path — the restore-from-file run consumes exactly this |
| S3 backup | display note | background uploader task ships closed segments + newest snapshot (they are already encrypted; the bucket learns sizes and timing only) |
| Restore runs | simulated log lines | the real implementations of the three ways, writing through `molt-storage::create` |

Every one of these keeps its existing command/tool surface — the co-equality
catalogue does not change shape, the mocks just gain organs.

## 5. Keys and encryption

* Per-workspace symmetric **workspace key** (32 B, random at creation), used
  with XChaCha20-Poly1305 for frames, snapshots and exports.
* The key is stored **sealed** in `keys/workspace.key`:
  * v1: sealed to a device key held in `~/.moltrepublic/device.key` (0600) —
    no passphrase prompt, honest threat model note: protects the synced/
    backed-up workspace dir, not a fully compromised home directory;
  * v2 (opt-in): passphrase sealing via argon2id — the parameters already
    have a home in the manifest's `[crypto]` table.
* The recovery seed (create-run) derives the workspace key via HKDF —
  that is what makes seed-based restore real: `key = HKDF(seed, "molt-ws-key",
  workspace_id)`.
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
| deleted while open elsewhere | flock prevents it locally; external deletion surfaces on next append as an I/O error, workspace closes with notice |

## 8. Testing

* **Round-trip**: create → write scripted event sequence → close → open →
  actor state equals a pure in-memory run of the same commands (the engine
  is already deterministic; this is the keystone test).
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

1. **S1** `molt-storage` crate: framing, crc, encryption, torn-tail
   recovery, round-trip + truncate-anywhere tests. No engine wiring.
2. **S2** create/open/close/delete wired (chat + proposals persist); LOCK;
   Open screen scans manifests. *(the “it survives a restart” demo)*
3. **S3** snapshots + startup replay budget; fsck tool.
4. **S4** manual `.molt.enc` export/import = the real restore-from-file.
5. **S5** S3 uploader task; restore-from-S3 real.
6. **S6** passphrase key sealing (opt-in).
