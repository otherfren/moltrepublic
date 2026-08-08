# Backup & Restore — design for S4/S5/S6

Design doc for the security-critical storage stories of `docs/ui/mock_todo.md`:

| Story | What | Milestone | Section |
|---|---|---|---|
| 9 | manual encrypted single-file workspace export (`.molt.enc`) | S4 | §3 |
| 13 (file half) | import / restore-from-file | S4 | §4 |
| 10 | at-rest passphrase sealing | S6 | §5 |
| 12 | auto-backup to S3 | S5 | §6 |
| 13 (S3 half) | restore-from-S3 | S5 | §6.6 |

Companion documents: `concept-workspace-storage.md` (directory layout, key
hierarchy, milestones), `persistent_chain.md` (what import must verify),
`recovery_ritual.md` (the safe re-join path this design deliberately does NOT
replace), `log_compaction.md` Teil B (pruned chains an import may meet).

Every open product question in this document is **flagged AND decided with a
recommended default** — an implementation agent is never blocked on a missing
decision. The consolidated list is §9.

---

## 1. Scope and non-goals

**In scope.** A versioned, passphrase-protected single-file export of a
workspace directory; its import counterpart with hard verification; real
at-rest sealing driven by the recovery phrase; an engine backup ticker that
ships the same blob to S3 with honest bookkeeping; restore-from-S3 as
download + the same import path.

**Non-goals.**

* **Multi-device / live cloning.** An export is a *backup of knowledge*, not a
  second live device. Live crypto state (MLS ratchets, SMP queue credentials)
  is deliberately excluded — §3.3 has the security argument. Rejoining the
  live republic after restoring goes through the **recovery ritual**.
* **The S3 HTTP client itself.** Story 5 (`NetTestS3`) builds the SigV4
  client in `molt-net` (pure-Rust signature, through the configured
  fail-closed/Tor-capable `Dialer`). This document assumes its interface
  (§7.2) and designs only what rides on it.
* **Provider-side bucket versioning / lifecycle rules.** We do our own
  retention (§6.3); anything the provider adds on top is out of scope.

## 2. Ground truth this design builds on (verified against the code)

* The workspace dir (concept §2): plaintext `manifest.toml` (id, name,
  created, m/n, `[crypto]` table), `prefs.toml`, `keys/workspace.key`
  (workspace key sealed to `~/.moltrepublic/device.key`), `keys/seed.sealed`
  (seed entropy device-sealed, own AAD domain `molt-seed-v1`), `log/*.mlog`
  (XChaCha20-Poly1305 frames, AAD = `workspace_id ‖ segment ‖ seq`),
  `snapshots/*.msnap`, `chain.state`, `transport.state`, `logo.<ext>`,
  `LOCK`, `tmp/`.
* The key hierarchy is **fully deterministic from the recovery phrase**:
  `seed = bip39::to_entropy(phrase)` (32 B, checksummed),
  `workspace_id = HKDF(seed, "molt-ws-id", member)`,
  `workspace_key = HKDF(seed, "molt-ws-key", workspace_id)`,
  `identity_sk = HKDF(seed, "molt-ws-identity", workspace_id)`.
  `chain.state` / `transport.state` use sub-keys HKDF-derived from
  `workspace_key` (`molt-chain-state` / `molt-transport-state`). This
  derivability is the load-bearing fact of the whole design: *whoever holds
  the phrase and the manifest id can reconstruct every storage key* — no
  sealed blob is ever the only copy of a derivable key.
* `verify_chain` (molt-engine) is hard-reject, all-or-nothing; pruned chains
  verify via `verify_suffix_chain` against the threshold-signed checkpoint
  blob (`ChainStateFile::Pruned`). A first prune raises the manifest version
  (`STORAGE_VERSION_PRUNED`) so older binaries refuse — the version-gate
  precedent §5.4 reuses.
* `transport.state` holds the MLS snapshot, mesh queue credentials
  (`Transport::export_creds`), delivery cursors, and the cached
  `identity_sk`; only a **clean close** persists the advanced ratchet.
* Crypto crates already in tree: `chacha20poly1305` (XChaCha — the house
  cipher, `CryptoParams::default().cipher == "xchacha20poly1305"`), `hkdf`,
  `sha2`, `bip39`, `getrandom`, `zeroize`, `hmac`, `subtle`, `bincode`,
  `crc32c`, `ed25519-dalek`. **No Argon2/scrypt/PBKDF2 is in the lock file**
  — the passphrase KDF is a new dependency (§3.4). The manifest already
  reserves `kdf = "argon2id"` in `[crypto]`.

## 3. S4 — the export blob: `.molt.enc`, format `molt-export-v1`

### 3.1 Design goals

1. **One file + one secret = restored knowledge.** After total device loss a
   user restores the readable history and the verified chain from the blob
   alone plus a secret they retain.
2. **Re-protection is real.** The log inside is already encrypted, but its
   key is device-sealed — useless off-device. The export re-keys protection
   under a secret that survives the device (a chosen passphrase for manual
   exports, the phrase-derived workspace key for automatic ones, §3.4).
3. **Safe to hand to an untrusted store.** The blob leaks nothing beyond the
   workspace-id pseudonym and its size; every byte is authenticated; a
   tampered or truncated blob is rejected as a whole.
4. **Never a live-state cloning vector.** See §3.3.

### 3.2 What is included, what is excluded

| File | In export? | Rationale |
|---|---|---|
| `manifest.toml` | **yes** | the identity card; import cross-checks it against the genesis |
| `prefs.toml` | **yes** | node-local prefs are part of the user's own backup (it is their machine's state, encrypted under their secret); `shared_files` paths are local-only info the owner already knows |
| `log/*.mlog` | **yes, verbatim ciphertext** | the history. Frames stay encrypted under `workspace_key` — the AAD binds `(workspace_id, segment, seq)`, and the import target has the same id and key, so the files are portable as-is; no decrypt/re-encrypt pass, no plaintext ever inside the blob |
| `snapshots/` — **newest valid snapshot only** | **yes** | speeds the first open after import; snapshots are droppable optimizations, so one is enough (smaller blob) |
| `chain.state` | **yes, verbatim ciphertext** | the threshold-signed chain — the part of the backup that is *verifiable*; import decrypts it (chain sub-key is derived from `workspace_key`) and hard-verifies (§4.2) |
| `logo.<ext>` | **yes** | applied org state materialized as a file |
| `keys/workspace.key` | **no** | sealed to the *exporting* device's key — dead weight anywhere else. The key itself travels inside the encrypted payload (§3.5) |
| `keys/seed.sealed` | **no** (the *entropy* may travel in the payload, §3.5) | same reason |
| `transport.state` | **NO — hard exclusion** | §3.3 |
| `LOCK`, `tmp/`, dot-files | **no** | runtime scratch |

Unknown extra files in the dir are skipped and named in the export result
(honesty: the user sees what the blob does not contain).

### 3.3 Security decision: MLS ratchet state and SMP queue credentials are NOT exported

The task is explicit about asking this question; the answer is a firm **no**,
for four independent reasons:

1. **Nonce/keystream reuse.** Restoring an old MLS snapshot on a second
   device (or on the same device *while the original still runs*) forks the
   ratchet: two devices advance the same sending chain independently and
   encrypt different plaintexts under the same `(key, nonce)` schedule. MLS's
   per-message `reuse_guard` protects against *replay within one state*, not
   against two live copies of the state. This is the catastrophic AEAD
   failure mode; no UX gain justifies it.
2. **Forward secrecy is deletion.** The MLS state's security property *is*
   that old key material gets deleted on advance (`transport.state` is
   overwrite-in-place for exactly this reason — concept §3.5). A backup that
   freezes ratchet state is a forward-secrecy time capsule: compromise of one
   old blob retroactively decrypts traffic the protocol promised was gone.
3. **SMP queue credentials are instance-bound.** A restored copy re-adopting
   the original's recipient keys either steals the queues from a
   still-running original or fights it for them; both are silent-failure
   modes (the transport section of CLAUDE.md documents how subtle these
   are). Queues are cheap to re-mint; identity is not queue possession.
4. **The safe path already exists and is proven.** The recovery ritual
   (`recovery_ritual.md`, Phase 4, E2E-tested) re-admits a seat by threshold
   approval, re-keys the MLS leaf, and serves the chain — that is the
   *designed* way back to liveness. The export restores **knowledge**; the
   ritual restores **membership**. §4.4 makes this boundary explicit to the
   user.

What DOES travel (inside the encrypted payload): the **derived key material**
`workspace_key` and (when available) the 32-byte **seed entropy**. Both are
*re-derivable* facts, not live protocol state — carrying them creates no
divergence hazard. The seed makes the blob a full seat-capability token
(§3.5 discusses the tradeoff); the workspace key alone makes it a readable
archive.

### 3.4 Key modes and KDF

Two key modes, marked in the plaintext header:

* **`key_mode = "passphrase"`** — manual exports (story 9). The user chooses
  an export passphrase at export time.
  * KDF: **Argon2id** via the RustCrypto **`argon2`** crate (pure Rust — new
    dependency, consistent with the pure-Rust posture; the manifest's
    `kdf = "argon2id"` field has reserved this since S1).
  * **Parameters (v1 defaults, written into the header):**
    `m_cost = 65536 KiB (64 MiB)`, `t_cost = 3`, `p = 1`, output 32 B,
    salt = 32 B fresh from `getrandom`. (RFC 9106's second recommended
    profile; `p = 1` keeps low-end devices and the actor-adjacent task
    simple. ~0.3–1 s on current hardware — felt, not painful.)
  * **Import caps** (DoS guard against a malicious header):
    `m_cost ≤ 1 GiB`, `t_cost ≤ 16`, `p ≤ 8`; reject beyond.
  * Passphrase policy: engine-enforced **minimum 10 characters** (length
    only, no composition rules); rejected with a clear `BadPayload`.
    Normalization: NFC of the passphrase string, UTF-8 bytes into Argon2.
* **`key_mode = "workspace"`** — automatic S5 backups (story 12), where no
  prompt is possible.
  `k_root = HKDF(workspace_key, "molt-export-backup-v1", workspace_id_raw)`.
  The engine holds `workspace_key` for every open workspace; the restoring
  user re-derives it from **recovery phrase + the workspace id** (which the
  bucket object key carries, §6.2) — full 256-bit entropy, so no memory-hard
  KDF is needed and no prompt ever happens during backup.
  *Rejected alternative:* a random backup key stored device-sealed — it dies
  with the device, which defeats the purpose of an off-device backup; stored
  anywhere else it becomes a new secret-management problem. The phrase is
  the one secret the product already requires users to retain.

From `k_root`, the actual stream key binds the exact header:

```
k_stream = HKDF(k_root, "molt-export-stream-v1", header_bytes)
```

where `header_bytes` is the verbatim plaintext header JSON. Any header
tampering (parameters, key mode, workspace id, chunk size) changes
`k_stream` and fails authentication on the first chunk — the header needs no
separate MAC.

### 3.5 Byte layout

```
.molt.enc :=
  magic       : b"molt-export-v1\0"          (15 B, repo tag convention)
  header_len  : u32le
  header      : JSON (plaintext, exactly header_len bytes)
  chunk*      : the encrypted payload stream

header (plaintext, minimal on purpose) :=
{ "format": "molt-export-v1", "version": 1,
  "workspace_id": "<64 hex>",                 // the dir identity (see below)
  "key_mode": "passphrase" | "workspace",
  "kdf": { "algo": "argon2id", "m_kib": 65536, "t": 3, "p": 1,
           "salt": "<64 hex>" },              // passphrase mode only
  "cipher": "xchacha20poly1305",
  "chunk_bytes": 4194304 }

chunk :=
  nonce       : 24 B  (random per chunk, getrandom)
  ct_len      : u32le
  ciphertext  : XChaCha20-Poly1305(k_stream, nonce, aad, plaintext-chunk)

aad(chunk i) := b"molt-export-v1\0" ‖ workspace_id_raw(32 B)
                ‖ i:u64le ‖ final:u8      // final = 1 on the last chunk
```

* **Chunking**: plaintext is cut at 4 MiB. The AAD's chunk index kills
  reorder; the `final` flag kills truncation (a stream that ends without a
  `final=1` chunk is rejected). Every non-final chunk must decrypt to
  exactly `chunk_bytes` — a short non-final chunk is rejected (no
  splice-shortening).
* **Why `workspace_id` and not `republic_id` in header + AAD** (deliberate
  deviation from the task sketch, security-argued): the republic id is the
  *shared* identity of the group — putting it in plaintext would let a
  storage provider **correlate different members' backups of the same
  republic**. The workspace id is a per-member pseudonym (HKDF of the
  member's own seed), already plaintext in `manifest.toml` and needed for
  the `workspace` key mode anyway. The republic id IS bound — one layer
  down: import recomputes it from the decrypted genesis and hard-verifies
  the chain against it (§4.2). Format version is in the AAD via the tag.
* **No plaintext creation timestamp** in the header (minimal-plaintext
  principle); it lives authenticated inside the payload meta.

**Payload** (concatenation of the decrypted chunks):

```
payload :=
  meta_len   : u32le
  meta       : JSON
  entry*     : file entries, paths sorted lexicographically (deterministic)

entry :=
  path_len   : u16le
  path       : UTF-8, relative, no "..", no leading '/'
  data_len   : u64le
  data       : raw file bytes

meta (encrypted+authenticated) :=
{ "created": <unix s>, "exporter": "<crate version>",
  "at_rest": "device" | "phrase",             // §5 state at export time
  "workspace_key": "<64 hex>",                // always present
  "seed": "<64 hex>" | null,                  // when available (§3.6)
  "files": <entry count> }
```

*Rejected alternatives for the container:* **tar** (in the lock only as a
transitive dep; carries mtimes/uids/orderings = metadata noise and
non-determinism, plus a long history of path-traversal foot-guns) and
**age/rage-style envelopes** (new dependency, no AAD position binding, no
chunk-truncation story). A 30-line bespoke framing we fully control beats
both. **aes-gcm** (in tree via OpenMLS) was rejected as the outer cipher:
XChaCha20-Poly1305 is the house cipher everywhere else in storage, and its
24-byte nonces make random-nonce-per-chunk safe without counter bookkeeping.

### 3.6 The seed-in-payload decision

`meta.seed` carries the raw 32-byte seed entropy **when the exporting side
has it** (unsealed workspaces: from `keys/seed.sealed`; S6-sealed
workspaces: never — the seed is not on disk, §5, and is deliberately not
prompted for).

* **Why include it (default: yes, when available):** the seed makes the
  blob a *complete* recovery artifact — after total loss (device AND the
  24-word phrase), blob + export passphrase re-derives `identity_sk`, so
  the user can still run the recovery ritual and reclaim their seat. Users
  lose phrases; a backup that silently isn't one is the kind of dishonesty
  this project forbids. It is also exactly the trust level `seed.sealed`
  already established on disk (decision 2026-07-15) — re-protected under
  Argon2id instead of the device key, which is not weaker.
* **The cost, stated plainly:** blob + passphrase = **full seat capability**
  (identity signing key derivable). The export UI must say so: "Dieses
  Backup + Passphrase ersetzt deine Recovery-Phrase. Behandle es wie sie."
* **Consistency pin:** whenever `seed` is present, export and import both
  verify `derive_workspace_key(seed, id) == workspace_key` and refuse a blob
  violating the hierarchy invariant.
* **S6-sealed workspaces** export with `seed = null`: the blob restores
  knowledge; the seat needs the phrase — which an S6 user by definition
  retains, because it is their unlock credential (§5).

Flagged as product question P1 (§9) with default **include when available**.

### 3.7 Integrity and authenticity of the blob

* Every byte after the magic is either key-binding input (header) or AEAD-
  authenticated (chunks). Bit-flips, splices, reorders, truncation → hard
  reject with a position-specific error.
* Authenticity is **symmetric** (whoever knows the secret can mint a valid
  blob). That is acceptable because the part of the payload that carries
  *republic truth* — the chain — is independently threshold-signed and
  hard-verified at import (§4.2): a passphrase holder can forge chat
  history in a blob (their own local, ephemeral data — they could edit
  their own disk just as well) but **cannot forge a single chain block**.
* **Rollback**: an attacker with write access to the storage location can
  substitute an older *genuine* blob. Import shows the authenticated
  `meta.created` age ("Backup vom …, N Tage alt") and the verified chain
  height before finalizing; there is no cross-device anchor to fully
  prevent rollback, and the doc says so honestly. (S5 retention keeps
  several generations, which helps detection, §6.3.)
* *Rejected for v1:* additionally signing the payload with `identity_sk`.
  The importer has no trusted pubkey before chain verification anyway
  (bootstrapping problem); after chain verification the roster is known and
  a signature check would only prove "a seat holder made this blob" — a
  marginal gain over the chain verify itself, not worth a second signing
  path (the founding-ritual lesson: never fork a second signing path).
  Noted as a possible v2 extension.

## 4. S4 — import semantics (story 13, file half)

### 4.1 Pipeline (two-phase, layering-clean)

`molt-storage` cannot call `verify_chain` (engine is a higher crate), so
import is **stage → verify → commit**:

1. **Stage (`molt-storage`)**: parse magic/header (unknown `version` →
   polite refusal, *before* any KDF work); derive `k_stream` (Argon2id caps
   enforced, §3.4); stream-decrypt chunks; validate entry paths against an
   **allowlist** (`manifest.toml`, `prefs.toml`, `chain.state`,
   `log/NNNNNN.mlog`, `snapshots/NNNNNNNNNNNN.msnap`, `logo.<ext>` — reject
   anything else, which subsumes traversal attacks); write everything into
   a dot-staging dir `root/.import-<id>/` (invisible to the scan);
   consistency-check the key hierarchy (§3.6); decrypt the genesis frame
   with the payload's `workspace_key` (proves key ↔ content match); decrypt
   and parse `chain.state`. Returns an `ImportStaging` handle exposing the
   parsed chain (`Full` blocks or `Pruned { checkpoint_blob, blocks }`),
   the manifest, and meta.
2. **Verify (`molt-engine`)** — *mandatory, hard-reject:*
   * `verify_chain` over full chains, `verify_suffix_chain` over pruned
     ones — bad sig, broken `prev`, height gap, below-threshold, repeated/
     unknown signer, double-apply, forged genesis id: **whole import
     rejected**, staging discarded, nothing materialized.
   * Republic-id consistency: the id recomputed from the genesis roster must
     match the chain's; manifest `name/rule_m/rule_n` must match the
     `Founded` genesis event (the manifest is the unauthenticated cover
     sheet — the genesis is authoritative).
   * Header `workspace_id` == manifest id == AAD id of the decryptable
     genesis frame.
3. **Commit (`molt-storage`)**: re-seal `workspace_key` under the **local**
   device key → `keys/workspace.key`; when `meta.seed` is present and
   `at_rest == "device"`, re-seal it → `keys/seed.sealed`; when seed is
   present, derive `identity_sk` and write a **fresh minimal**
   `transport.state` (version + identity seed only — derived, not cloned,
   so no replay hazard; it lets chain governance sign after reopen and
   feeds the recovery flow); atomic rename staging → final dir. `abort()`
   removes the staging dir; a crash leaves only a dot-dir the next startup
   sweep deletes (same pattern as `.create-*`).

A sealed (`at_rest == "phrase"`) export **round-trips sealed**: commit
writes *no* device-sealed key material; the imported workspace lands in the
encrypted-at-rest state and opens only per §5.

### 4.2 Failure honesty

Each rejection carries its layer: "wrong passphrase / damaged blob" (AEAD
chunk failure — deliberately NOT distinguished from tampering; the AEAD
cannot tell and we do not guess), "unsupported format version", "blob is
internally inconsistent (key hierarchy)", "chain verification failed:
<verify_chain reason>", "a workspace with this id already exists". No
partial directory ever becomes visible.

### 4.3 Collision with an existing workspace of the same id

Default: **refuse**. The existing dir may be *ahead* of the backup (newer
chain head, newer chat); silently replacing live state with an older copy is
data loss. The refusal message offers the explicit escape hatch: the GUI/MCP
may re-run the import with `replace = true`, which first moves the existing
dir to `.trash/` (the recoverable 30-day path `DeleteWorkspace` already
uses) and then commits. Never an in-place merge — two logs of the same
workspace cannot be merged (concept §2: file-level merging forks history).
Flagged as P2 with default **refuse + explicit trash-then-replace**.

### 4.4 What identity does the importer have afterwards? (the honest boundary)

**Import restores knowledge. Recovery restores membership.** Concretely,
after import + open:

* **Has**: the full verified chain (governance state, roster, org state),
  the chat/event history, prefs, the workspace key — and, seed permitting,
  the seat's identity signing key.
* **Has not**: an MLS leaf in the live group, mesh queues, delivery
  cursors. The republic's running group has long ratcheted past any state
  this node ever had; there is no safe way to "resume" — and we excluded
  that state from the blob on purpose (§3.3).
* Therefore the imported workspace opens in a **detached** state: reading
  everything works; the mesh does not come up (no `transport.state` mesh
  creds — `reopen_transport` finds none and the open path skips mesh
  bootstrap with a session notice instead of failing). The notice says
  exactly this: "Aus Backup wiederhergestellt — Wissen ist da, Mitgliedschaft
  nicht. Wiederbeitritt über Recovery-Link." The recovery ritual then
  re-admits the seat (`Membership{Restored}` block, MLS re-key) — the
  identity key for the seat proof comes from the imported seed or the typed
  phrase. GUI affordance (a "Recover" button on the detached notice) is a
  UI follow-up, not part of these stories.

## 5. S6 — at-rest passphrase sealing (story 10)

### 5.1 Design: the phrase is the credential; sealed = no key material on disk

`mock_todo` Finding 10 says "den device-sealed Log-Key **zusätzlich** unter
der Recovery-Phrase versiegeln". Checked against molt-storage reality, a
phrase-sealed *copy* of the workspace key would be pure redundancy: the
workspace key is **HKDF-derivable from the phrase + the plaintext manifest
id** (§2). Storing a second sealed blob whose content anyone with the phrase
can compute adds a consistency surface and zero security. The design
therefore implements the *intent* (phrase-gated at rest, `seed.sealed`
removed, real verification) with **derive-and-verify** instead of a second
sealed file — a deliberate, flagged deviation (P3):

* **Encrypted-at-rest state** := `manifest.toml [crypto] sealed = "phrase"`
  (new field, `#[serde(default = "device")]` — additive) **and** the absence
  of `keys/workspace.key` and `keys/seed.sealed`.
* **EncryptWorkspace** (durable, real): refuse for the ACTIVE workspace (as
  today); require the recovery phrase in the command and **verify it first**
  (derive → decrypt genesis frame) — a user must prove they hold the
  credential *before* we delete their only other way in; then delete
  `keys/workspace.key` + `keys/seed.sealed` (best-effort overwrite-then-
  unlink; honest note: on modern filesystems/SSDs secure deletion is not
  guaranteed — the threat model is the synced/backed-up dir, same as
  today's), set `sealed = "phrase"`, bump the manifest version (§5.4).
  `Command::EncryptWorkspace` gains a `phrase` field (additive).
* **DecryptWorkspace** (durable, real — keeps the existing toggle
  contract): parse the phrase (BIP-39 checksum catches typos *before* any
  crypto), `seed = entropy(phrase)`, `key = HKDF(seed, "molt-ws-key",
  manifest.id)`, attempt AEAD-open of the genesis frame (segment 1, seq 1)
  — **the Poly1305 tag is the real phrase verification**; on success
  re-seal `workspace.key` + `seed.sealed` under the local device key, set
  `sealed = "device"`, restore the version floor. Wrong phrase → clean
  `Crypto` error, nothing changed on disk.
* **Open/close while sealed**: `OpenWorkspace` on a sealed entry keeps
  refusing with "decrypt first" (the existing GUI contract). Close is
  unaffected — sealing is only togglable on closed workspaces, so
  open/close never meet a half-sealed dir. A later "unlock for this session
  only" mode (open with phrase, stay sealed on disk) is flagged P4 —
  default: not in v1, the toggle semantic ships first.
* `transport.state` and `chain.state` need no migration: their sub-keys
  derive from `workspace_key`, which the phrase re-derives.

**Verification-oracle note:** deriving-and-trying against the genesis frame
gives an attacker with the dir exactly the brute-force interface the AEAD
already gives them on any frame — nothing new is exposed. The phrase is 256-
bit entropy; brute force is out of reach, which is also why no Argon2 is
needed here (Argon2id stays reserved in `[crypto].kdf` for a possible future
*weak-password* mode — deliberately not built now, P5).

### 5.2 Scan / state derivation (survives restarts)

`scan_workspaces` reports `encrypted := (manifest.crypto.sealed == "phrase")`
— derived from the directory, so it survives restarts (fixing "Neustart
vergisst den Zustand"). For sealed entries the details panel hides seed and
roster (`read_sealed_seed` / `peek_genesis` return `None` because the key
material is absent — that behavior is already designed in and falls out for
free). A dir whose marker and key files disagree (says "device" but
`workspace.key` is missing) is corruption: scan lists it, open fails with a
clear `BadFile` — never a guess.

### 5.3 Migration of existing workspaces

None needed for the default state: existing manifests lack the field →
serde-defaults to `"device"`, which is exactly what they are. The state
changes only through the (new, real) Encrypt/Decrypt commands. The GUI's
existing mock texts get truth-updates (molt-ui/mcp: wording only).

### 5.4 Version gate

Encrypting bumps `manifest.version` to `STORAGE_VERSION_SEALED` (next
capability version after `STORAGE_VERSION_PRUNED`, same precedent:
`bump_pruned_version`), so an **older binary refuses politely**
(`NewerVersion`) instead of tripping over a keyless dir with a raw I/O
error. Decrypting recomputes the floor (pruned chain present → pruned
version, else base) — a decrypted workspace stays openable by older
binaries when nothing else requires newness.

## 6. S5 — auto-backup to S3 (story 12) and restore-from-S3 (story 13)

### 6.1 The blob is the S4 blob

One format, two key modes. Auto-backups use `key_mode = "workspace"`
(§3.4): no prompt, restorable with phrase + bucket. Manual "Backup jetzt
erstellen" (story 9's GUI modal) writes a `key_mode = "passphrase"` file to
the chosen local path via the same code path.

**Consistency of an export taken while the workspace is open:** log
segments are append-only framed files — a concurrent append can at worst
leave a partial last frame in the copy, which the import's torn-tail
handling truncates (crash-consistent semantics, same guarantee a hard crash
has); `chain.state`, `prefs.toml`, snapshots are atomic-rename files, so a
read sees an old or a new version, never a torn one. Therefore backup
**never pauses the writer** and contends with nothing. Sealed (S6) +
**open** workspaces back up fine (the engine holds `workspace_key`); sealed
+ **closed** workspaces are skipped with an honest per-workspace status
("versiegelt — wird beim nächsten Öffnen gesichert"), because no key is
accessible and prompting from a ticker is not a thing (P6).

### 6.2 Object naming

```
s3://<bucket>/molt/<workspace_id>/<unix_ts>.molt.enc
```

* `workspace_id`: the per-member pseudonym (§3.5's correlation argument —
  never the republic id, never the display name).
* `unix_ts`: seconds at export start, zero-padded to 12 digits
  (`001752800000`) so lexicographic key order equals age order forever —
  the retention pruner sorts keys, nothing parses timestamps back.
* No provider-versioning reliance; each backup is its own object.

### 6.3 Retention (keep-copies pruning)

After a **confirmed** upload: `list_prefix("molt/<id>/")`, sort by key,
delete oldest objects beyond `s3_keep_copies` (config, default already
wired). Deletion failures are surfaced as a notice, never retried silently
into the next generation mismatch — the next successful backup re-prunes.
Pruning runs in the same off-actor task as the upload (never on the actor).

### 6.4 Ticker lifecycle (engine actor pattern) and honest stamps

* One global backup ticker task, spawned with the other run tickers: every
  60 s it sends the engine-internal `Command::BackupTick`.
* The **synchronous** `cmd_backup_tick` handler only *decides*: for each
  workspace with `prefs.s3_backup`, S3 configured, `now - last_backup ≥
  s3_interval_min`, not already in `backup_inflight`, and key-accessible
  (§6.1) → mark inflight, `tokio::spawn` the backup task (build blob to a
  temp file → S3 PUT via the story-5 client through the fail-closed dialer
  → prune). The handler never awaits I/O — same pattern as every net task.
* The task reports back as engine-internal commands:
  `NetBackupDone { id, ts, object, bytes }` → engine sets
  `prefs.last_backup = ts` (persisted via the prefs path that already
  exists), updates the list entry, clears inflight.
  `NetBackupFailed { id, error }` → session notice + list state, stamp
  **untouched**, inflight cleared.
* **`cmd_set_workspace_backup` loses its fake stamp** (`last_backup_min =
  0` on enable — mock_todo Finding 12): enabling now only persists the pref
  and lets the next `BackupTick` run a real first backup; the stamp moves
  **only on `NetBackupDone`**. "Letztes Backup: gerade eben" becomes a
  statement of fact or does not appear.
* Failure honesty end to end: no fake success anywhere; a failing bucket
  shows a red state and an aging "last backup" — exactly what is true.

### 6.5 Orphan listing (story 8) and the backup table

`Command::BackupList` (a **tool** — a human/agent refreshes the table)
spawns a task listing `molt/` one prefix level deep; reply comes back as
internal `NetBackupListed { entries }` — per prefix: workspace id, newest
object ts, count, total bytes. Entries whose id matches no local workspace
are the **orphans** (real ones, replacing the static demo rows —
`SessionView::default` drops its fake `BackupOrphan`s in the same
change-set). Restore-from-S3 (§6.6) starts from exactly this listing.

### 6.6 Restore-from-S3

`RestoreStart { way: "s3", target }` becomes real: resolve target (empty →
newest object of the chosen orphan/workspace; explicit object key
otherwise), download via the S3 client to a temp file, then **the exact S4
import pipeline** (§4) with `key_mode = "workspace"`: the user is prompted
for their **recovery phrase**; `workspace_key = HKDF(entropy(phrase),
"molt-ws-key", <id from the object key>)`. `RestoreStart` gains a `secret`
field (additive; phrase for S3-way, export passphrase for file-way). The
mock ticker dies: `RestoreTick` is **removed** from the command set (it was
INTERNAL machinery; the INTERNAL list shrinks by one), replaced by real
progress events from the task — internal `NetRestoreProgress { pct, line }`
(download 0–60, KDF+decrypt+verify 60–90, materialize 90–100; every log
line reports something that actually happened), `NetRestoreDone`,
`NetRestoreFailed`. `RestoreCancel` aborts the task (inbound download —
abort is safe here; nothing outbound is in flight) and cleans staging.
`RestoreFinish` opens the imported workspace into the detached state of
§4.4.

### 6.7 Credentials and what the provider learns (threat model, stated honestly)

* **Creds** live in `config.toml` (already wired), plaintext on disk 0600 —
  the same trust level as `device.key`: an attacker with the home dir has
  both. Bucket creds allow **deleting/overwriting/rolling back** backups
  and reading blob *ciphertext*; they never allow reading content (that
  needs phrase or passphrase). Config-file hardening is a global follow-up,
  not this story.
* **The provider (and a network observer past Tor) learns:** that backups
  happen, their timing/cadence (the interval is visible), object sizes
  (≈ workspace growth), and the workspace-id pseudonym. It learns **no
  names, no content, no membership, no republic identity** — and two
  members of the same republic backing up to the same provider are **not
  linkable by object naming** (distinct workspace ids). Size/timing padding
  (bucketed sizes, jittered schedule) is a possible refinement — P7,
  default off in v1; the doc states the leak instead of hiding it.
* All S3 traffic goes through the configured dialer: Tor-capable,
  **fail-closed** (story-5 contract) — a Tor-mode node never falls back to
  clearnet for a backup.

## 7. Interfaces — module boundaries and the command surface

### 7.1 `molt-storage` (format + sealing; no engine knowledge)

```rust
// export.rs
pub enum ExportKey { Passphrase(String), WorkspaceDerived }   // §3.4
pub struct ExportOutcome { pub bytes: u64, pub skipped: Vec<String>, pub created: u64 }
/// Closed dir (unseals via device key; sealed dirs refuse without `phrase_override`)
pub fn export_dir(ws_dir: &Path, root: &Path, key: &ExportKey,
                  out: &mut dyn Write) -> Result<ExportOutcome, StorageError>;
/// Open workspace (engine holds the key; crash-consistent copy, §6.1)
pub fn export_open(ws: &OpenedWorkspaceView, key: &ExportKey,
                   out: &mut dyn Write) -> Result<ExportOutcome, StorageError>;

// import.rs
pub enum ImportSecret { Passphrase(String), Phrase(String) }  // maps to key_mode
pub struct ImportStaging {
    pub manifest: WorkspaceManifest,
    pub checkpoint: Option<CheckpointState>,   // pruned chains
    pub chain: Vec<ChainBlock>,
    pub meta: ExportMeta,                      // created, at_rest, seed presence
    // staging dir path; not yet visible to any scan
}
pub fn import_stage(root: &Path, input: &mut dyn Read,
                    secret: &ImportSecret) -> Result<ImportStaging, StorageError>;
impl ImportStaging {
    /// After the ENGINE verified the chain (§4.1 step 2).
    pub fn commit(self, root: &Path, replace: bool) -> Result<PathBuf, StorageError>;
    pub fn abort(self);
}

// sealing.rs (S6)
pub fn seal_at_rest(root: &Path, ws_dir: &Path, phrase: &str) -> Result<(), StorageError>;
pub fn unseal_at_rest(root: &Path, ws_dir: &Path, phrase: &str) -> Result<(), StorageError>;
pub fn is_sealed(manifest: &WorkspaceManifest) -> bool;
```

Secrets (`passphrase`, derived keys, seed buffers) are `zeroize`d on drop
(`zeroize` is in tree). Argon2 runs inside these functions — they are
blocking and must only ever be called off-actor (`spawn_blocking` /
spawned tasks), documented on the functions.

### 7.2 `molt-net` — assumed story-5 S3 client (interface contract only)

```rust
pub struct S3Client { /* endpoint, bucket, region, creds, dialer */ }
impl S3Client {
    pub async fn put_object(&self, key: &str, body: PathOrBytes) -> Result<(), NetError>;
    pub async fn get_object(&self, key: &str, out: &mut File,
                            progress: impl Fn(u64, Option<u64>)) -> Result<(), NetError>;
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<S3Entry>, NetError>; // key,size,ts
    pub async fn delete_object(&self, key: &str) -> Result<(), NetError>;
}
```

SigV4 with a pure-Rust HMAC-SHA256 (`hmac` + `sha2` are in tree), all
connects through `Dialer` (fail-closed). Note for story 5: `config.toml`
has no region key — SigV4 needs one; default `"us-east-1"` + optional new
config key `s3_region` (additive, `#[serde(default)]`).

### 7.3 `molt-core` — commands/events (additive-only), co-equality

| Command | Surface | Notes |
|---|---|---|
| `ExportWorkspace { id, dest, passphrase }` | **tool** `export_workspace` + GUI modal | human decision; replaces the no-op modal (story 9) |
| `EncryptWorkspace { id, phrase }` | **tool** (exists; gains `phrase`, additive) | real S6 seal |
| `DecryptWorkspace { id, phrase }` | **tool** (exists) | real verification |
| `SetWorkspaceBackup { id, enabled }` | **tool** (exists) | loses the fake stamp |
| `BackupNow { id }` | **tool** `backup_now` | manual S5 trigger, same task as the ticker |
| `BackupList` | **tool** `backup_list` | refreshes table + orphans (story 8) |
| `RestoreStart { way, target, secret }` | **tool** (exists; gains `secret`, additive) | file + s3 ways become real |
| `RestoreCancel` / `RestoreFinish` | **tools** (exist) | unchanged contract |
| `BackupTick` | **INTERNAL** | the ticker's heartbeat (replaces nothing) |
| `NetBackupDone / NetBackupFailed` | **INTERNAL** | task → engine; an MCP agent must not forge a backup stamp |
| `NetBackupListed` | **INTERNAL** | listing result |
| `NetExportDone / NetExportFailed` | **INTERNAL** | export task result (export runs off-actor: Argon2 + file I/O) |
| `NetRestoreProgress / NetRestoreDone / NetRestoreFailed` | **INTERNAL** | real progress; `RestoreTick` is **deleted** from `Command` and from the INTERNAL list |

Every row lands in `crates/molt-mcp/src/lib.rs` (`tools()` or `INTERNAL`) in
the same change-set — `co_equality_every_command_is_a_tool_or_documented_internal`
enforces it. MCP note: tool params carry secrets (passphrase/phrase); the MCP
operator is a co-equal *trusted* operator by architecture — noted, not
mitigated here.

New/changed core types (all additive): `WorkspaceInfo` honesty fields as
needed (`backup_state: u8` optional), `RestoreState` loses mock fields it no
longer needs (keep shape, change semantics), `CryptoParams.sealed:
String` (default `"device"`), `SessionView::default` drops demo
`BackupOrphan`s.

### 7.4 `molt-engine`

* `cmd_export_workspace` / `cmd_backup_now` / `cmd_backup_tick` /
  `cmd_backup_list`: synchronous validate-and-spawn; results return as the
  INTERNAL commands above (ticker/net-task pattern, never awaiting I/O on
  the actor).
* `cmd_encrypt_workspace` / `cmd_decrypt_workspace`: validate (not active,
  known id), then `spawn_blocking` on the sealing functions, result via a
  small internal ack path (or synchronous if measured cheap — genesis-frame
  decrypt is one AEAD; **decision: synchronous is acceptable**, matches
  open/create which already run on the actor at current sizes).
* Restore: `cmd_restore_start` dispatches per way (file → staging task; s3
  → download+staging task); the task calls engine-side verify via a
  returned `ImportStaging` handed back inside `NetRestoreProgress`'s
  completion variant — concretely: the task sends an internal
  `NetRestoreStaged { staging }`, the **handler** runs `verify_chain`
  (synchronous, pure CPU) and either commits (spawned, then
  `NetRestoreDone`) or aborts with the reason.
* Detached-open: `cmd_open_workspace` on a dir with no mesh creds skips
  mesh bootstrap and sets the §4.4 notice instead of erroring.

### 7.5 Work packages (three implementation agents, no further product decisions needed)

* **Agent A — story 9 + file-import (S4):** molt-storage `export.rs` /
  `import.rs` (+ `argon2` dep), core commands `ExportWorkspace` +
  `RestoreStart.secret` + internal export/restore events, engine handlers +
  file-way restore + detached-open, GUI/MCP wiring, tests §8.1.
* **Agent B — story 10 (S6):** molt-storage `sealing.rs`, `CryptoParams.
  sealed`, version gate, scan derivation, engine encrypt/decrypt real,
  GUI/MCP truth-texts, tests §8.2. Independent of A except the shared
  `at_rest` marker in the export meta (string constant).
* **Agent C — stories 12+13-S3 (S5):** depends on A's format and story 5's
  client. Engine ticker + backup tasks + retention + `BackupList`/orphans +
  honest stamps, s3-way restore = download + A's pipeline, demo-orphan
  removal, tests §8.3/§8.4.

## 8. Test plan — the red tests that pin the invariants (TDD anchors)

### 8.1 Story 9 / S4 (export + file import)

1. **Round-trip keystone**: create → scripted events → export (open AND
   closed variants) → import into a fresh root → open → state equals the
   original run (reuses the determinism keystone comparator).
2. **Wrong passphrase**: import fails with the §4.2 message; **no staging
   residue**, no visible dir (assert directory listing unchanged).
3. **Tamper rejection**: flip any single byte (header, each chunk, payload
   region) → hard reject; property-style loop over offsets.
4. **Truncation**: cut the file at every chunk boundary and mid-chunk →
   reject (missing final flag / short chunk).
5. **Chain-verify-on-import**: blob whose chain has one forged block
   (re-signed by a non-roster key / height gap / doctored genesis) →
   import refused at the verify phase, staging aborted. Pruned-chain
   variant via `verify_suffix_chain`.
6. **Path traversal / allowlist**: crafted payload entries (`../x`,
   absolute, `keys/…`, `transport.state`) → rejected.
7. **Key-hierarchy pin**: blob whose `seed` does not derive its
   `workspace_key` → rejected (both directions: export refuses to build
   one, import refuses to accept one).
8. **Exclusion pin**: a built blob never contains `transport.state`
   bytes/entry (regression fence for §3.3).
9. **Collision**: same-id import refused; `replace = true` trashes then
   commits; trash entry recoverable.
10. **KDF caps**: header demanding `m_cost` above cap → polite refusal
    before allocation.
11. **Sealed round-trip**: export of a phrase-sealed dir carries
    `at_rest = "phrase"`, `seed = null`; import lands sealed.

### 8.2 Story 10 / S6

1. **State survives restart**: encrypt → `scan_workspaces` reports
   `encrypted = true` in a fresh process; open refused with "decrypt
   first".
2. **Real phrase verification**: wrong phrase (valid BIP-39, wrong words) →
   `Crypto` error, dir unchanged (byte-compare); typo'd phrase → BIP-39
   checksum error *before* any file is touched.
3. **Right phrase**: decrypt restores `workspace.key` + `seed.sealed`
   (device-unsealable), `sealed = "device"`, version floor recomputed;
   subsequent open works and state equals pre-seal state.
4. **No key material while sealed**: after encrypt, assert
   `keys/workspace.key` and `keys/seed.sealed` are absent and
   `read_sealed_seed`/`peek_genesis` return `None`.
5. **Active refusal**: encrypting the open workspace → `WorkspaceBusy`.
6. **Encrypt requires proof**: encrypt with a wrong phrase → refused,
   nothing deleted.
7. **Version gate**: sealed fixture with `STORAGE_VERSION_SEALED` → old
   reader path (`version > supported`) refuses politely (golden-fixture
   style).

### 8.3 Story 12 / S5 backup

1. **No fake success**: upload failure (mock S3 returns 500 / dialer
   refuses) → `last_backup` unchanged, notice set, list shows honest age;
   `SetWorkspaceBackup(enabled=true)` alone **never** stamps.
2. **Stamp only on confirmation**: mock S3 200 → `NetBackupDone` →
   `prefs.last_backup` persisted; restart shows real age.
3. **Retention**: with `keep_copies = 3` and 5 objects, the 2 oldest are
   deleted, newest 3 remain (mock listing, order pinned).
4. **Ticker due-logic** (pure unit tests on the handler): interval not
   elapsed → no spawn; inflight → no second spawn; sealed+closed →
   skipped with status; disabled → never.
5. **Orphans real**: mock bucket with one foreign prefix → `BackupList`
   populates exactly one orphan; `SessionView::default` contains none.
6. **Crash-consistent open-export**: export while a writer appends →
   import succeeds with a prefix of the log (torn tail truncated), never
   an error.

### 8.4 Story 13 / S5 restore

1. **E2E over a loopback mock S3** (in-process HTTP stub, like the SMP
   loopback posture): backup → wipe local dir → restore-from-S3 with the
   phrase → workspace state equals the backed-up prefix; progress events
   strictly monotonic and matching real phases.
2. **Failure honesty**: abort download mid-stream → `NetRestoreFailed`,
   no dir, no staging residue, run view shows the true failure line.
3. **Detached-open**: restored workspace opens with the §4.4 notice, no
   mesh; recovery ritual from this state re-admits the seat (extends the
   existing two_instances recovery E2E).
4. **`RestoreTick` removal**: co-equality test compiles/passes with the
   variant gone and the new INTERNAL entries present.

## 9. Recommended defaults and flagged product questions

Defaults chosen throughout; the genuinely product-flavored ones:

* **P1 — seed in manual exports** (§3.6). Default: **included when
  available**, with the explicit UI warning that blob+passphrase equals
  the recovery phrase. Alternative (knowledge-only blobs) rejected because
  a backup that cannot recover a seat after phrase loss silently under-
  delivers on "backup".
* **P2 — import collision** (§4.3). Default: **refuse; explicit
  `replace = true` trashes the existing dir first** (30-day recoverable).
* **P3 — S6 mechanism** (§5.1). Default: **derive-and-verify, no
  phrase-sealed key file** (deviation from mock_todo's "zusätzlich
  versiegeln" letter, same intent, argued in place).
* **P4 — unlock-for-session-only** (§5.1). Default: **not in v1**; the
  existing durable Encrypt/Decrypt toggle contract ships first.
* **P5 — weak-password sealing mode** (§5.1). Default: **not built**;
  `[crypto].kdf = "argon2id"` stays reserved for it.
* **P6 — sealed+closed auto-backup** (§6.1). Default: **skip with honest
  status** (no ticker prompts, no key caching).
* **P7 — size/timing padding toward the S3 provider** (§6.7). Default:
  **off in v1**, leak documented.
* **Argon2id parameters** (§3.4): 64 MiB / t=3 / p=1, caps 1 GiB / 16 / 8.
* **Passphrase policy** (§3.4): min 10 chars, length-only.
* **Chunk size** (§3.5): 4 MiB.
* **Cipher** (§3.5): XChaCha20-Poly1305 (house cipher); KDF crate:
  RustCrypto `argon2` (new, pure Rust).
* **Object naming/retention** (§6.2–6.3): `molt/<workspace_id>/<ts:012>.molt.enc`,
  prune to `s3_keep_copies` after confirmed upload.
* **MLS/SMP live state**: **never exported** (§3.3) — the one
  recommendation in this document that should be treated as load-bearing
  rather than a tunable default.

## 10. S7 — restore lands SEALED (ratified + BUILT 2026-08-08)

Keystones: `restore_real::backup_fetch_lands_sealed_and_the_phrase_opens_it`
(fetch → sealed stub → wrong-phrase refusals sync+async → verified open),
`backup_ticker::the_uploaded_blob_never_carries_workspace_plaintext`.
As built: the command is `BackupFetch`; the stub rides `SEALED_RESTORED`
in the manifest (`WorkspaceInfo.restored` additive); the open path is the
existing `DecryptWorkspace`, whose restored arm drives the file-restore
pipeline with `replace` (the stub is trashed only after the verify).

Product decision (user, 2026-08-08): the Welcome card "Restore from backup"
routes to **Settings › Backup**. The table there restores the **newest**
remote version of a selected workspace **without asking for any secret**;
the result lands locally **still encrypted** (the user has entered no seed,
so there is nothing to decrypt with — and if it arrived decrypted we would
have proof of plaintext on the bucket, which
`backup_ticker::the_uploaded_blob_never_carries_workspace_plaintext` exists
to make impossible). Afterwards the GUI offers jumping to the Open list —
the exact view "Open a local workspace" shows. Decryption happens THERE, on
open, with the recovery phrase.

Mechanism (reuses S4/S5 machinery, no new crypto):

* **Fetch** — new co-equal command `BackupFetch { id }` (a human decision:
  GUI button in the backup table, MCP tool). Downloads the newest
  `molt/<id>/<ts>.molt.enc` object and writes it **verbatim** to
  `<workspace_root>/<id>.<ts>.restored.molt.enc`. No decrypt, no staging,
  no chain verification yet — the file is ciphertext byte-for-byte as
  uploaded.
* **Scan** — `scan_workspaces` lists such artifacts as entries in the
  "restored backup, sealed" state: shown under the id pseudonym + the
  backup timestamp (the NAME is inside the ciphertext, by design — showing
  it would prove the leak this whole section forbids).
* **Open** — opening such an entry prompts for the recovery phrase (the
  same posture as an at-rest-sealed workspace) and then drives the
  EXISTING file-restore pipeline (`RestoreStart{way:"file"}` semantics:
  phrase → workspace key → `read_export` → hard chain verification →
  materialize), deleting the artifact on success. A wrong phrase refuses
  honestly and leaves the artifact in place.
* **Welcome** — the "Restore from backup" card navigates to Settings tab
  Backup; the `rw-mode == "s3"` wizard branch retires (the phrase step
  moved to open-time; `RestoreStart{way:"s3"}` stays on the MCP surface).

Tests (red first): fetch lands the exact uploaded bytes as a sealed
artifact and never plaintext; the scan lists it sealed; open with the right
phrase materializes through the verified pipeline and removes the artifact;
a wrong phrase refuses and keeps it.
