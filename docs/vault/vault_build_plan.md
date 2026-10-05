# Vault: build plan

Status: OPEN PLAN (2026-10-05). Executes `vault_threshold_disclosure.md`
(rev 3, D1-D16) stage by stage. Every stage below is written for one
subagent; read the spec in full before starting any stage. The spec is
authoritative for behaviour; where this plan deviates (section 1) the S0
contract commit amends the spec in the same change, so the two never
disagree on master.

Ground truth: master `ca9c556a`. Line numbers below are hints from that
commit; re-locate by symbol before editing.

## 0. Rules for every stage

- Work on master, or in a git worktree only when the stage runs beside
  another one (section 3). A worktree agent runs `git merge master` first,
  uses its OWN `CARGO_TARGET_DIR` (`<worktree>/target`, `<worktree>/target/dev-ui`
  for the UI), and is merged back to master and deleted before the stage
  is reported done. After every merge, check master HEAD and rerun the
  stage's verification on master.
- `git add <paths>` only - never `git add -A`. Commit at every green
  checkpoint, `git push` after each commit. Trailer:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- TDD: the red tests listed per stage are written first and seen failing
  for the stated reason.
- Builds: `-j 2`, per crate. Never `cargo test --workspace`, never the
  full Slint window build, never `cargo build -p molt-app` in the normal
  profile. molt-ui only via the live-preview chain:
  `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test -j 2 -p molt-ui --lib --features molt-ui/live-preview`
  and `scripts/dev-ui.sh build` as the .slint check. `free -g` before an
  engine test build when another stage is running.
- clippy at 0 per touched crate: `cargo clippy -j 2 -p <crate> --all-targets`
  (molt-ui: `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo clippy -j 2 -p molt-ui --all-targets --features molt-ui/live-preview`).
  Tests use `.expect("…")`, never `.unwrap()`.
- UI text: one fact per string, no em dash anywhere a human reads,
  strings in BOTH `i18n.rs` and `theme.slint` `Strings` (parity test).
  Logs as `key=value`.
- Nothing from a live republic in tests or fixtures; names `a`, `b`,
  `c`, `d`, texts like `one` / `two`.
- A stage that finds the contract (S0) insufficient stops and reports;
  the orchestrator lands a small contract amendment on master first.
  A UI stage never edits `molt-core`, `molt-engine` or `molt-net`.

## 1. Decisions this plan takes (and the spec amendments S0 lands)

### 1.1 Where the crypto lives: a new crate `molt-vault`

`crates/molt-vault`: Shamir/Feldman dealing and combining, HPKE share
seal/open, payload AEAD, vault-key derivation, complaint decision,
deposit signing/verification. Pure functions, no I/O, randomness passed
in by the caller. Depends on `molt-core` (record types and canonical
byte layouts) and the crypto crates only. `molt-engine` depends on it.

Why not the alternatives:
- `molt-core`: holds the canonical byte layouts (sha2 only, like
  `roster_canonical_bytes`), but stays RNG-free and free of curve
  arithmetic; the layouts live in a new `molt-core/src/vault.rs`.
- `molt-net`: the transport crate, the scope of the ring-free guard and
  of the C secp256k1 exception; vault crypto has nothing to do with
  transport and would ride its 400+ dependency build for every test.
- `molt-storage`: at-rest encryption; the vault key derivation needs
  X25519 and HPKE, which storage does not have.
- A separate crate builds and tests in seconds, keeps the new
  dependencies in one auditable slice, and gets its own pure-Rust guard.

Layering becomes: core -> config -> storage -> net -> **vault** -> engine
-> mcp -> ui -> app (`molt-vault` depends on core only; the arrow just
says the engine sits above it). The workspace `Cargo.toml` header lists
the crate.

### 1.2 Library verdicts (spec §13; verified against the sources 2026-10-05)

| need | verdict | why |
|---|---|---|
| Shamir + Feldman over Ristretto | **`vsss-rs` 5.1.0**, `default-features = false`, features `curve25519` (+ `zeroize`; S1 trims the rest) | 5.1.0 rides `curve25519-dalek` 4.1.3, `elliptic-curve` 0.13, `group` 0.13, `rand_core` 0.6 - all already in `Cargo.lock`. 6.0.x moved to `curve25519-dalek` 5 / `ff` 0.14 / `rand_core` 0.10 and would duplicate the whole curve stack beside the OpenMLS-pinned 4.1.3. Caller x-coordinates: `ParticipantIdGeneratorType::List`. Feldman: `Feldman::split_secret_with_participant_generator_and_verifiers`, `FeldmanVerifierSet::verify_share`, `combine`. Ristretto wrapper: `vsss_rs::curve25519::{WrappedRistretto, WrappedScalar}`. Apache-2.0 OR MIT. Enables the `group`, `group-bits`, `rand_core` features of the existing `curve25519-dalek` (additive). Spec §13 named 6.x; S0 amends. |
| HPKE with caller ephemeral | **`hpke` 0.13.0** (rozbb), `default-features = false`, features `x25519`, `alloc` | `setup_sender` / `single_shot_seal` take a caller `csprng`; the KEM's `encap` calls `gen_keypair(csprng)`, which fills exactly one private-key-sized `ikm` from the RNG and runs RFC 9180 `DeriveKeyPair(ikm)` (`src/kem.rs` `gen_keypair`, `src/kem/dhkem.rs` `encap`). A one-shot RNG that yields exactly `ikmE` (and panics on a second draw) therefore gives the spec's deterministic ephemeral with no fork and no hand-rolled HPKE. Same RustCrypto generation as the tree (`chacha20poly1305` 0.10, `hkdf` 0.12, `sha2` 0.10, `x25519-dalek` 2); `rand_core` 0.9.5 is already in the lock. MIT/Apache-2.0. |
| rejected: `hpke-rs` 0.6.1 | in tree via OpenMLS, but no caller ephemeral: `encaps` draws from the private `prng`; the only override is the `hpke-test-prng` feature, which unifies into OpenMLS and makes every HPKE instance draw from a 256-byte fake buffer. Never. |
| rejected: `hpke` 0.14.x | new RustCrypto generation (`chacha20poly1305` 0.11, `hkdf` 0.13, `sha2` 0.11, `rand_core` 0.10) - a second copy of every primitive. |
| vault keypair | `hpke` `X25519HkdfSha256::derive_keypair(ikm)` | one key type end to end; no direct `x25519-dalek` use needed. |
| payload AEAD | `chacha20poly1305` 0.10 `XChaCha20Poly1305` (in tree) | |
| deterministic dealing RNG | `rand_chacha` 0.3.1 (`ChaCha20Rng::from_seed`, rand_core 0.6, in lock) | feeds `vsss-rs`. |
| deposit signature | `ed25519-dalek` 2 (in tree) | the identity key, as everywhere. |

Consequence: complaints ARE decidable; the spec's undecided fallback
(§7 last paragraph) is not taken.

### 1.3 Spec deviations (S0 amends the spec text)

1. **Vault key salt: the founding `nostr_pk`, not `republic_id`** (spec §5).
   `republic_id` hashes the final name and every seat's anchors and is
   first computed in `maybe_seal`, after every join; the charter features
   are chosen only at `CreatePropose`, also after every join. A joiner
   therefore cannot derive a key salted with `republic_id` when it sends
   its join. New formula:
   `vault_ikm = HKDF-SHA256(entropy, info = "molt-vault-x25519-v1" ‖ founding_nostr_pk ‖ identity_pk)`,
   `(vault_sk, vault_pk) = DeriveKeyPair(vault_ikm)`.
   The founding `nostr_pk` is ticket-salted with a random ticket, so one
   phrase gets a different vault key in every republic (the property the
   spec wanted from `republic_id`); it is fixed in the genesis and in
   `CheckpointState.founding_identities` for life, so a recovered seat
   (new WORKING `nostr_pk`, same phrase) re-derives the same key from the
   chain. One join round, no protocol change. Every vault-capable joiner
   ALWAYS sends `vault_pk` (it cannot know the charter yet); the founder
   drops the field from the table when the charter has no vault.
2. **`vault_ikm` is persisted** in `TransportState.vault_seed`
   (`#[serde(default)] Option<Vec<u8>>`, beside `nostr_sk`) at founding,
   join, recovery and restore, because the phrase is not available at
   runtime (`seeds` is cleared when a workspace is sealed at rest).
   Ephemerals derive from it: `ikmE_i = HKDF(vault_ikm, "molt-vault-eph-v1" ‖ secret_id ‖ seat_i)`
   (spec §7 step 4 said "seed"; same secrecy, re-derivable from the phrase).
3. **Deterministic dealing.** The depositor must answer a complaint with
   `share_i` (spec §7) yet stores nothing and cannot open the holders'
   HPKE shares. So the deposit carries a public random `nonce` (16 bytes,
   hex), and the polynomial is dealt from
   `deal_seed = HKDF(vault_ikm, "molt-vault-deal-v1" ‖ republic_id ‖ name ‖ kind ‖ nonce)`
   (`s` = wide reduction of `HKDF(deal_seed, "molt-vault-secret-scalar-v1")`,
   coefficients from `ChaCha20Rng::from_seed(deal_seed)`). The depositor
   re-deals to answer a complaint, and recovers `s` to re-seal without
   re-typing the text. Trust-model line added to spec §4: **the
   depositor's phrase opens its own deposits.** This is not new: under
   rev 3 the seed-derived `ikmE` already lets that phrase recompute every
   ephemeral secret and decrypt every share of the deposit.
4. **Receipts, complaints, reveals, answers are authenticated by the MLS
   sender credential**, the established control-frame rule (`by != from`
   -> drop; the credential key IS the identity key, bound at join by
   `key_package_binding`). No extra Ed25519 layout. A complaint's verdict
   does not rest on who sent it: anyone recomputes it from the reveal.
5. **`seat_i` in every AAD/info is the holder's member name**
   (le32-prefixed); its Shamir x-coordinate is its 1-based position in the
   genesis founding table.
6. **Checkpoint of a vault republic: one variant, one tag.** Every cut in
   a republic whose genesis is roster-v6 is `ChainChange::CheckpointVault
   { upto, state_hash }` and hashes under `molt-chain-checkpoint-v10`
   (v9 layout - explicit group count, feature presence byte - plus
   `vault_pk` as a fourth field in BOTH identity tables, closing the gap
   that `founding_identities` is the only record of the keys after the
   genesis is pruned). The vault group is always folded to one
   `vault_base` entry; the wiki fold inside it stays content-selected as
   today. A `Checkpoint` / `CheckpointFolded` in a v6-genesis chain is
   refused (once folded, always folded, trivially). The proposer and the
   receiver pick the variant from their own genesis, so
   `WorkspaceEvent::CheckpointProposed` needs no new wire field.
7. **Real vault iff the genesis is roster-v6.** `effective_features()`
   (a union with applied `set_features`) never decides it. A v5 roster
   with `vault` keeps the mock surface forever: the pane shows one line
   (`needs a vault republic`), no deposit button; the generic
   `seal_secret` op keeps working there only for history. In a v6
   republic the generic `propose` on `vault` is refused (`use vault_seal`);
   deposits and grants come only from the vault commands.
8. **Validity is a projection rule, not a block rule, for grants.** A
   grant block whose `secret_id` is not the current version at its height
   commits but is void (nobody answers, the card says `void`). Deposit
   blocks, by contrast, are hard-rejected by `verify_chain` when malformed
   or when `sig_depositor` fails.
9. **Join MAC unchanged.** `vault_pk` rides `JoinRequest` with
   `#[serde(default)]`; on Nostr the gift-wrap is signed by the joiner's
   transport key, and on every path the member's sign-what-you-see check
   of its own `vault_pk` is the binding (as spec §6 says).

## 2. Contract (fixed by S0 before anything fans out)

All additive (`#[serde(default)]`, `skip_serializing_if` where the field
is new on an existing type).

### 2.1 molt-core

- `MemberIdentity.vault_pk: String` (`default`, `skip_serializing_if = "String::is_empty"`).
- `TransportState.vault_seed: Option<Vec<u8>>`,
  `TransportState.vault_status: VaultStatusStore` (receipts and reveal
  outcomes, last-wins per holder: `BTreeMap<secret_id, BTreeMap<MemberId, VaultReceipt>>`
  + `BTreeMap<secret_id, BTreeMap<MemberId, VaultRevealOutcome>>`).
- `PublishJob.vault: Option<String>` (payload or base hash; routes the
  trickle like `wiki_base`).
- `molt_core::vault` module:
  - `VaultDeposit { depositor, name, kind, m: u8, holders: Vec<String>, commitments: Vec<String>, enc_share: Vec<String>, payload: VaultPayloadRef { hash, size }, nonce: String, sig_depositor: String }`
    (hex strings; field order = canonical order).
  - `VaultGrant { grant_id, secret_id, reader }`.
  - Applied payload ops on `Surface::Vault`: `{"op":"deposit", ...VaultDeposit}`,
    `{"op":"grant", ...VaultGrant}`; fold entry `{"op":"vault_base","hash","size"}`.
  - limits: `VAULT_PAYLOAD_MAX = 102_400`, `VAULT_NAME_MAX = 64`,
    `VAULT_KIND_MAX = 24` chars; `check_vault_name`, `check_vault_kind`
    (trimmed, non-empty, no control chars).
  - no layout fns yet: S1 adds them to the same file.
- `Command`: `VaultSeal { name, kind, text }`, `VaultReseal { secret_id }`,
  `VaultGrant { secret_id, reader }`, `VaultRead { secret_id }` (Seat
  tools); INTERNAL `NetVaultPayloadFetched { hash, bytes, generation }`,
  `NetVaultPayloadFailed { hash, generation }`,
  `NetVaultBaseFetched { bytes, generation }`, `NetVaultBaseFailed { generation }`,
  `NetVaultReceipt { from, frame, generation }`, `NetVaultReveal { … }`,
  `NetVaultResp { … }`, `NetVaultAsk { … }` (frame structs from molt-net
  are mirrored as plain fields, the `NetMirrorDecl` pattern).
  `NetJoinRequested` gains `vault_pk: String` (`default`).
- `Reply::VaultText { secret_id, name, kind, text }`,
  `Reply::VaultPending { secret_id, have: u8, need: u8 }`.
- `Event::VaultReadable { secret_id }` (UI refresh when answers complete).
- `MoltError::VaultBasePending(WikiBaseProgress)` and
  `MoltError::Vault(String)` (one compact reason: `needs 2 <= m <= n-2`,
  `founding only`, `needs a newer version: <seat>`, `not verified`,
  `payload not held`, `not the reader`, `no vault`, `use vault_seal`,
  `too large`).
- `SurfaceSnapshot.vault: Option<VaultView>` (Vault surface only):

```text
VaultView {
  real: bool,                       // genesis is roster-v6
  m: u8, n: u8,
  base_pending: Option<WikiBaseProgress>,
  deposits: [VaultDepositView {
    secret_id, depositor, name, kind, size: u64,
    state: "pending" | "committed" | "sealed" | "hardened",
    proposal: Option<u64>,          // pending proposal id
    replaces: Option<String>,       // secret_id of the version it replaces
    verified: u8, holders: u8,      // receipts counted / n-1
    readable_by: u8,                // m minus valid revealed shares
    complaints: [{ holder, status: "open" | "bad_share" | "false" }],
    mine: bool, held: bool,         // payload held here
    my_check: "ok" | "bad" | "none" | "pending",
  }],
  grants: [VaultGrantView {
    grant_id, secret_id, depositor, name, reader,
    state: "pending" | "committed" | "void",
    proposal: Option<u64>, at: Option<u64>, mine: bool,
  }],
}
```

  pinned by a serde JSON shape test in molt-core.
- `UiSnapshot.vault_rows: u32` (`default`) for `gui_over_mcp`.
- `Surface::is_implemented(Vault)` -> true (flip
  `engine/src/tests/workspace_tests.rs` assert).

### 2.2 molt-net

- `invite::JoinRequest.vault_pk: String` (`default`); carried through
  `nostr_ritual.rs` / `founding.rs` inbound into `NetJoinRequested`.

### 2.3 molt-engine (stubs only)

- Dispatch arms for every new Command in `lib.rs`, calling handlers in a
  new `src/vault/mod.rs` (`cmd_vault_seal`, `cmd_vault_reseal`,
  `cmd_vault_grant`, `cmd_vault_read`, `cmd_net_vault_*`). Until their
  stage lands they return `Err(MoltError::FeatureDisabled("vault"))` /
  `Reply::Ack` for INTERNAL ones. Empty submodule files exist with owner
  comments: `vault/deposit.rs` (S3b), `vault/receipts.rs` (S3c),
  `vault/grant.rs` (S4), `vault/read.rs` (S4), `vault/fold.rs` (S5).
- `snapshot(Vault)` fills `vault: Some(VaultView { real: false, .. })` via
  `self.vault_view()` in `vault/mod.rs`.

### 2.4 molt-mcp

- Seat tools `vault_seal`, `vault_reseal`, `vault_grant`, `vault_read`
  with schemas and descriptions; INTERNAL entries for every `NetVault*`;
  `INTERNAL` array length bumped; `present()` for `VaultText` /
  `VaultPending`. Stale texts fixed: the `vault seal_secret` mentions
  (instructions, `propose` description and schema, `select_view` list,
  `create_propose` features: `vault` = founding-only real surface,
  needs 2 <= m <= n-2).
- The read scope set stays unchanged (`the_read_scope_is_exactly_this_set`).

## 3. Stage map and parallelism

```text
S0 contract ──┬─ S1 crypto ──── S2 founding ── S3a wire+plane ── S3b deposit ──┬─ S3c receipts/complaints ─┐
              │                                                                └─ S4 grant/read ───────────┤
              └─ U1 wizard/org lock ── U2 vault pane (fixture-fed) ─────────────── U3 real-engine GUI + lab ┤
                                                                     S5 fold/cut/recovery/backup ──────────┤
                                                                                       S6 final gate ─────┘
```

Waves (at most two stages building at once; each row is one wave):

| wave | stage A (master or worktree) | stage B (worktree) | why disjoint |
|---|---|---|---|
| W0 | S0 contract | - | everything depends on it |
| W1 | S1 crypto | U1 wizard + org lock | S1: `crates/molt-vault`, `molt-core/src/vault.rs`, root `Cargo.toml`/`Cargo.lock`. U1: molt-ui + .slint only |
| W2 | S2 founding | U2 vault pane | S2: engine ritual/chain/proposals, core roster bytes. U2: molt-ui + .slint only |
| W3 | S3a wire + payload plane | U2 (continued, if not done) | S3a: molt-net, molt-storage, engine net/ |
| W4 | S3b deposit | - | touches proposals/governance/verify; nothing else safe beside it except UI |
| W5 | S3c receipts/complaints | S4 grant/read | only after S3b committed the `vault/mod.rs` hooks; S3c owns `vault/receipts.rs`, S4 owns `vault/grant.rs` + `vault/read.rs` + `crates/molt-mcp/tests/vault_tools.rs` |
| W6 | S5 fold/cut/recovery/backup | U3 real-engine GUI tests + lab | S5: core chain.rs, engine chain/, storage export/import, recovery; U3: molt-ui tests, `scripts/vault_lab.py` |
| W7 | S6 final gate | - | |

Pairs allowed to run concurrently, and nothing else: (S1, U1), (S2, U2),
(S3a, U2), (S3c, S4), (S5, U3). The U-track never edits core/engine/net;
any contract gap stops the UI stage (rule 0). The lab seam that U3 needs
(`vault-lab` feature) is built by S3c, so U3 starts after S3c merged.

Merge order inside a wave: the stage whose files are lower in the
layering merges first; the second agent then `git merge master` in its
worktree and reruns its verification before merging.

---

## S0 - Contract commit

- **Goal:** everything in section 2, compiling and green, plus the spec
  amendments of section 1.3; no behaviour change.
- **Spec:** §6 (record fields), §11 (tools), §5 amended.
- **Owns:** `crates/molt-core/src/lib.rs`, `crates/molt-core/src/vault.rs`
  (types only; S1 takes it over), `crates/molt-net/src/invite.rs`,
  `crates/molt-engine/src/lib.rs` (dispatch), `crates/molt-engine/src/vault/**`
  (stubs), every `MemberIdentity { .. }` literal site (43; list in the
  mapper notes: core 11, engine founding/recovery/ritual/tests,
  molt-storage 6), `crates/molt-mcp/src/lib.rs`,
  `docs/vault/vault_threshold_disclosure.md` (rev 4 status + §4, §5, §6,
  §7, §9.3, §12, §13 amendments of section 1.3).
- **Red tests first:**
  - core `vault_view_json_shape_is_pinned` - the JSON of a filled
    `VaultView` equals a literal (the UI's contract).
  - core `member_identity_without_vault_pk_serializes_as_before` - a
    `MemberIdentity` with empty `vault_pk` round-trips to the exact
    pre-change JSON (old wire shape, old snapshot files).
  - core `roster_bytes_are_unchanged_by_the_vault_pk_field` - the v4/v5
    pins still hold with `vault_pk` empty.
  - mcp `co_equality_every_command_is_a_tool_or_documented_internal`
    goes red the moment the Commands exist, green with the tools/INTERNAL.
  - mcp `vault_tools_are_seat_scope` - the four tools are `Scope::Seat`.
  - engine `workspace_tests` vault `is_implemented` assert flipped.
- **Steps:** types -> fix literal sites (`vault_pk: String::new()`) ->
  Command/Reply/Event/Error -> engine stubs -> MCP tools/INTERNAL/text ->
  spec amendment.
- **Done:** all per-crate suites below green, clippy 0, spec rev 4
  committed in the same commit series.
- **Verify:**
  `cargo test -j 2 -p molt-core && cargo test -j 2 -p molt-net --lib && cargo test -j 2 -p molt-storage && cargo test -j 2 -p molt-mcp && cargo test -j 2 -p molt-engine --lib`;
  `cargo check -j 2 -p molt-engine --tests`; clippy for core, net,
  storage, engine, mcp; `scripts/dev-ui.sh build` (Command enum
  consumers in molt-ui still compile).

## S1 - Crypto core and byte layouts

- **Goal:** `molt-vault` complete and the canonical layouts pinned.
- **Spec:** §5, §6, §7 steps 2-4, §7 complaint decision, §8 step 3-4, §13, §14.1.
- **Owns:** `crates/molt-vault/**` (new), `crates/molt-core/src/vault.rs`,
  root `Cargo.toml` (`[workspace] members`, `[workspace.dependencies]`
  `vsss-rs = { version = "=5.1.0", default-features = false, features = ["curve25519", "zeroize"] }`,
  `hpke = { version = "=0.13.0", default-features = false, features = ["x25519", "alloc"] }`,
  `rand_chacha = "0.3"`), the header comment, `Cargo.lock`.
- **Core layouts (`molt_core::vault`, all le32-length-prefixed fields,
  entry-counted tuples, NUL-terminated tag):**
  - `vault_key_info(founding_nostr_pk, identity_pk)` - `molt-vault-x25519-v1`
  - `secret_id(republic_id, &VaultDeposit)` - `molt-vault-secret-v1` over
    republic id, depositor, name, kind, m, holders, commitments, payload hash
    (not `enc_share`, not `nonce`, not the sig)
  - `deposit_signing_bytes(republic_id, &VaultDeposit)` - `molt-vault-deposit-v1`
    over every field except `sig_depositor`, `enc_share` and `nonce` included
  - `grant_id(secret_id, reader, proposal_id)` - `molt-vault-grant-v1`
  - `share_aad(secret_id, seat)` - `molt-vault-share-v1`
  - `resp_aad(republic_id, grant_id, seat)` - `molt-vault-resp-v1`
  - `payload_aad(republic_id, depositor, name, kind)` - `molt-vault-payload-v1`
  - `dek_info(republic_id, depositor, name, kind)` - `molt-vault-dek-v1`
  - `eph_info(secret_id, seat)` - `molt-vault-eph-v1`
  - `deal_info(republic_id, name, kind, nonce)` - `molt-vault-deal-v1`
  - `vault_base_canonical_bytes(&VaultBase)` - `molt-vault-base-v1`
    (entry-counted deposits in `(depositor, name)` order, each followed by
    its entry-counted grants in `grant_id` order; record = canonical JSON
    bytes as in the checkpoint groups). Decoder `decode_vault_base` beside it.
- **molt-vault modules:**
  - `key.rs`: `derive_vault_seed(entropy, founding_nostr_pk, identity_pk) -> Zeroizing<[u8;32]>`,
    `vault_keypair(seed) -> (sk, pk_hex)`, `canonical_vault_pk(&str) -> Result<String>`
    (64 lowercase hex, and a fixed-ikm test encapsulation to it succeeds -
    HPKE refuses an all-zero DH, so low-order points fail through the library).
  - `rng.rs`: `IkmRng` (rand_core 0.9 `RngCore + CryptoRng`, yields its 32
    bytes once, panics on any further draw).
  - `share.rs`: `deal(deal_seed, m, xs) -> (s, shares, commitments)`,
    `verify_share(commitments, x, share)`, `combine(m, [(x, share)]) -> s`.
  - `seal.rs`: `seal_share(vault_pk, share, aad, ikmE) -> 80 bytes`,
    `open_share(vault_sk, enc, aad)`, `seal_resp(reader_pk, share, aad, rng)`,
    `open_resp`.
  - `payload.rs`: `dek(s, info)`, `encrypt_payload(dek, text, aad, nonce24)`,
    `decrypt_payload`.
  - `deposit.rs`: `build_deposit(input, vault_seed, signing_key, rng) -> (VaultDeposit, payload_ct)`
    (validates name/kind/size, deals, seals, signs);
    `verify_deposit_shape(&dep, genesis_identities, m)` (holders = every
    seat but the depositor in genesis order, counts m / n-1, hex shapes);
    `verify_deposit_sig(&dep, republic_id, depositor_identity_pk)`;
    `check_my_share(&dep, republic_id, my_name, my_x, vault_sk) -> Result<Share, ShareFault>`;
    `rederive_share(&dep, republic_id, vault_seed, holder) -> (share, ikmE)` (depositor side);
    `recover_secret(&dep, republic_id, vault_seed) -> s` (re-seal).
  - `complaint.rs`: `decide(&dep, republic_id, holder, holder_vault_pk, share, ikmE) -> Outcome { Lie, BadShare, FalseComplaint }`.
  - `read.rs`: `read(&dep, republic_id, shares: &[(x, share)], payload_ct) -> Result<String, ReadFault { bad_seats }>`.
- **Red tests first** (each names the invariant it pins):
  - `byte_pins_*` one per layout above (golden hex for fixed inputs) -
    a layout change without a tag bump goes red.
  - `layouts_are_injective_across_field_boundaries` - `("ab","c")` vs
    `("a","bc")` differ for every two-string layout.
  - `vault_key_differs_per_founding_anchor_and_is_stable` - same phrase,
    two `nostr_pk` -> two keys; same inputs -> same key.
  - `ikm_rng_draws_exactly_one_ikm` - sealing a share asks the RNG for
    exactly 32 bytes (a hpke upgrade that draws more panics the test).
  - `share_ciphertext_is_reproducible_from_share_and_ikm` - the complaint
    recomputation is deterministic.
  - `dealing_is_deterministic_and_pinned` - fixed `deal_seed` -> golden
    commitments (guards vsss-rs RNG consumption drift).
  - `any_m_shares_reconstruct_and_m_minus_one_do_not` (2-of-4, 3-of-5, holders' x = 1-based genesis positions, depositor's x skipped).
  - `a_tampered_share_fails_feldman_and_names_its_seat`.
  - `complaint_outcomes_name_the_right_party` - honest reveal -> FalseComplaint;
    bad dealt share -> BadShare; lying reveal -> Lie.
  - `a_forged_depositor_fails_the_signature`; `a_reordered_holder_list_fails_the_shape_check`.
  - `payload_aad_binds_name_and_kind` - decrypt under another name fails.
  - `resp_aad_binds_grant_and_seat` - an answer for grant A does not open as grant B.
  - `max_payload_is_100_kib_plus_40` and `oversize_text_is_refused`.
  - `canonical_vault_pk_rejects_uppercase_short_and_low_order`.
  - `tests/pure_rust_guard.rs`: `cargo tree --locked -p molt-vault -e no-dev --target all -i ring` and `-i cc` are empty (pattern of `molt-engine/tests/c_free_guard.rs`).
- **Done:** all above green; `cargo tree -d -p molt-vault` shows no
  second `curve25519-dalek`/`chacha20poly1305`/`sha2`/`hkdf`; the
  existing ring-free guard still green.
- **Verify:** `cargo test -j 2 -p molt-vault && cargo test -j 2 -p molt-core && cargo clippy -j 2 -p molt-vault --all-targets && cargo clippy -j 2 -p molt-core --all-targets && cargo test -j 2 -p molt-net --test ring_free_guard && cargo test -j 2 -p molt-engine --test c_free_guard`.

## S2 - Founding: keys in the roster, bounds, lockout

- **Goal:** a vault founding seals a roster-v6 genesis whose every seat
  carries a verified `vault_pk`; bounds and D11/D14 enforced at every door.
- **Spec:** §3, §6 (vault key in the roster, lockout), §14.2; plan 1.3.1, 1.3.2, 1.3.7.
- **Owns:** `crates/molt-core/src/lib.rs` (`roster_canonical_bytes` +
  pins), `crates/molt-engine/src/{founding.rs, ritual_member.rs, nostr_ritual.rs, lifecycles.rs}`,
  `crates/molt-engine/src/chain/{verify.rs, projection.rs, test_support.rs}`,
  `crates/molt-engine/src/proposals.rs` (set_features only),
  `crates/molt-engine/src/net/ingest.rs` (set_features wire twin),
  `crates/molt-engine/Cargo.toml` (+ `molt-vault`),
  `crates/molt-engine/tests/vault_support/mod.rs` (new: `found_n_at(root, url, n, m, features)` over `MockRelay`, generalizing `nostr_recovery.rs::found_three_at`),
  `crates/molt-engine/tests/vault_founding.rs` (new), `wiki_export.rs` tag comment.
- **Steps:**
  1. Roster v6: tag `molt-roster-v6\0` iff any member has a non-empty
     `vault_pk`; layout = v5 with `put_bytes(vault_pk)` as the fourth field
     of each member run; v6 requires `features` Some. v4/v5 byte-identical.
  2. Founder: `start_ritual` derives its own vault seed (identity_pk +
     its founding nostr_pk); `MemberSeat::derive` likewise; `JoinRequest`
     sends `vault_pk`; `cmd_net_join_requested` normalizes it with
     `canonical_vault_pk` (empty allowed = older joiner), checks
     uniqueness including the founder.
  3. `cmd_create_propose` with `vault`: refuse unless `2 <= m <= n-2`
     (`needs 2 <= m <= n-2`) and every seat sent `vault_pk`
     (`needs a newer version: <seat>`). `full_identities` carries
     `vault_pk` only when the charter has `vault`.
  4. Member: `verify_seal_proposal` gets its own `vault_pk`; checks own
     value == own derivation, every other `vault_pk` canonical and unique,
     bounds, and `v6 <=> features ∋ vault <=> every seat has vault_pk`.
     `check_roster_anchors` extended; `verify_sealed_roster` likewise;
     the genesis byte-equality closure covers it unchanged.
  5. `verify_genesis`: for a v6 genesis require the bounds and every
     identity's `vault_pk`.
  6. Persist `vault_seed` into `TransportState` at seal (founder) and
     materialize (joiner); `JoinOutcome` carries it like `nostr_sk`.
  7. `set_features`: `vault` refused at `validate_org_payload`, at
     `cmd_propose`, at `cmd_approve`, and on the wire ingest
     (`founding only`).
  8. Engine helper `is_vault_republic()` (genesis / `founding_identities`
     carry vault keys) - the ONLY switch for the real vault (1.3.7).
- **Red tests first:**
  - core `roster_v6_byte_pin` and `roster_v6_separates_the_vault_field`;
    `vaultless_roster_stays_v5_byte_identical` (existing pins untouched).
  - founding unit: `verify_seal_proposal_rejects_a_tampered_own_vault_pk`,
    `..._rejects_a_duplicate_vault_pk`, `..._rejects_a_malformed_vault_pk`,
    `..._rejects_vault_outside_the_bounds` (2-of-3, 1-of-4),
    `..._rejects_vault_keys_without_the_vault_feature` and the converse,
    `verify_sealed_roster_rejects_a_swapped_vault_pk`.
  - `create_propose_refuses_vault_for_an_older_joiner` (a join without
    `vault_pk`) -> `needs a newer version: b`.
  - `create_propose_refuses_vault_at_2_of_3` -> `needs 2 <= m <= n-2`.
  - `verify_genesis_rejects_a_v6_genesis_missing_a_key`.
  - `set_features_refuses_vault_at_propose_approve_and_wire`.
  - integration `tests/vault_founding.rs`:
    `a_vault_founding_round_trips_roster_v6` (2-of-4 over MockRelay: every
    seat's genesis is v6, every seat's `vault_seed` derives its roster
    `vault_pk`), `a_vaultless_founding_stays_byte_identical` (2-of-4
    without vault: tag v5, no `vault_pk` in any identity).
- **Done:** all red tests green; full `molt-engine` suite green; the
  founding/two_instances/nostr_founding suites untouched in behaviour.
- **Verify:** `cargo test -j 2 -p molt-core && cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_founding --test founding --test nostr_founding --test two_instances && cargo test -j 2 -p molt-engine && cargo clippy -j 2 -p molt-core --all-targets && cargo clippy -j 2 -p molt-engine --all-targets`.

## S3a - Vault wire and payload plane

- **Goal:** the four vault control frames exist end to end (registered,
  parsed, sunk into `NetVault*` commands) and payload files travel as
  their own file-plane family with mandatory holding.
- **Spec:** §7 (receipts as control frames), §8.3 (answers), §9.2.
- **Owns:** `crates/molt-net/src/{vault_frames.rs (new), supervisor.rs, group_runtime.rs, file_plane.rs, trickle.rs, lib.rs}`,
  `crates/molt-net/tests/frame_disjointness.rs`,
  `crates/molt-storage/src/lib.rs` (vault payload sink, segment markers),
  `crates/molt-engine/src/net/{vault_payload.rs (new), mod.rs, files.rs (one hook), delivery.rs (one tick line)}`,
  `crates/molt-engine/src/transfer.rs` (fetch spawn),
  `crates/molt-engine/src/lib.rs` (FilePlane slots only).
- **Steps:**
  1. `vault_frames.rs`: tags `\x00molt-vrcpt-v1` (receipt/complaint
     `{v, by, secret_id, verdict: "verified"|"complaint", rev}`),
     `\x00molt-vrevl-v1` (`{v, by, secret_id, holder, share, ikm}`),
     `\x00molt-vresp-v1` (`{v, by, grant_id, seat, enc}`),
     `\x00molt-vask-v1` (`{v, by, grant_id}`); `to_frame`/`from_frame`,
     size caps; register in `CONTROL_FRAMES`, `MlsDecode`, the
     group-runtime dedup list and dispatch (`by != from` -> drop), the
     mesh-supervisor arm list, `EngineSink` default no-ops, `CmdSink`
     -> `Command::NetVault*`.
  2. Payload family: `vault_payload_key(rotation_seed, hash)` (info
     `molt-vault-payload-v1`), `vault_payload_series(hash)`;
     `StateStore::vault_piece(hash, index)`; trickle route on
     `PublishJob.vault`.
  3. Storage: `VAULT_BASE_SEGMENT = MAX-5`, `VAULT_PAYLOAD_SEGMENT = MAX-6`,
     `RESERVED_SEGMENT_FLOOR` lowered; `write_vault_payload(secret_id, bytes)`,
     `read_vault_payload`, `read_vault_payload_piece`, `remove_vault_payload`
     at `vault/<secret_id>.bin` under `hkdf32(ws_key, "molt-vault-payload", secret_id ‖ hash)`;
     writer messages + async loaders as for the wiki base.
  4. Engine `net/vault_payload.rs`: `vault_payload_tick()` from the 1 s
     delivery tick: for every current or pending deposit whose payload is
     not held, one fetch at a time per hash (`RETRY_EVERY_SECS = 15`);
     `cmd_net_vault_payload_fetched` checks hash and size, persists,
     emits; `serve_vault_pieces` answers `PieceWanted` before the
     share/mirror/cap gates in `cmd_net_piece_wanted`; never touches
     `MirrorState`, `mirror_dir`, quota or `FileCap`.
  5. `enqueue_vault_publish(hash)` for the depositor (S3b calls it).
- **Red tests first:**
  - net `vault_frames_round_trip_and_reject_wrong_version`,
    `vault_frame_tags_are_disjoint` (frame_disjointness),
    `a_vault_frame_whose_by_is_not_the_sender_is_dropped` (group_runtime unit).
  - storage `a_vault_payload_round_trips_sealed_at_rest`,
    `two_vault_files_cannot_swap` (file A renamed to B fails to decrypt),
    `segment_floor_ignores_the_vault_markers`.
  - engine `a_vault_piece_is_served_with_the_mirror_off_and_cap_zero`,
    `the_vault_payload_tick_fetches_a_missing_payload` (unit with a fake
    store), and integration `tests/vault_plane.rs`
    `a_payload_reaches_every_seat_without_mirror_consent` (2-of-4 vault
    republic from `vault_support`, mirror off on all seats, a payload
    published by one seat lands at the other three).
- **Verify:** `cargo test -j 2 -p molt-net --lib && cargo test -j 2 -p molt-net --test frame_disjointness --test file_plane --test trickle --test ring_free_guard && cargo test -j 2 -p molt-storage && cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_plane --test wiki_base_plane --test mirror_gossip`; clippy net, storage, engine.

## S3b - Deposit: propose, verify-gated approve, apply, view

- **Goal:** `vault_seal` / replace work end to end; approve is offered
  only after share AND payload verify; `verify_chain` rejects forged or
  malformed deposits; `read_state(vault)` shows real deposits.
- **Spec:** §6 (Deposit), §7 steps 1-5 and approval, §8.2 (supersede on
  replace), §10 first four bullets, §11.
- **Owns:** `crates/molt-engine/src/vault/{mod.rs, deposit.rs}`,
  `crates/molt-engine/src/proposals.rs` (Vault hooks in `cmd_propose`,
  `cmd_approve`, `try_apply`, `snapshot`), `crates/molt-engine/src/net/ingest.rs`
  (Vault shape+sig validation), `crates/molt-engine/src/chain/{governance.rs, checkpoint.rs (apply hook only), verify.rs (fold_one vault check)}`,
  `crates/molt-engine/tests/vault_deposit.rs` (new).
- **Steps:**
  1. `cmd_vault_seal`: refuse unless `is_vault_republic()`; build via
     `molt_vault::build_deposit` (fresh `nonce`, OS RNG for the AEAD
     nonce); persist payload; `enqueue_vault_publish`; propose the
     `deposit` payload through the internal propose path (generic
     `propose` on Vault in a v6 republic: `use vault_seal`). Same name
     by the same depositor = replace (`replaces` in the view).
  2. `cmd_approve` on a Vault deposit: base-pending -> `VaultBasePending`;
     not held -> `payload not held`; own share fails -> `not verified`;
     the depositor's own approve needs only the held payload.
  3. Wire ingest and `verify_chain` (`fold_one` for `Surface::Vault`
     in a vault republic): shape (`verify_deposit_shape`) and
     `verify_deposit_sig`; a failing deposit block hard-rejects the chain.
     Grants: shape only (`grant_id` recomputes, reader is a seat).
  4. `after_vault_applied` hook wired in all three apply sites
     (`proposals.rs` try_apply, `governance.rs` after_block_applied,
     `checkpoint.rs` candidate adoption); a deposit that replaces drops
     the old payload file and supersedes pending grants on the old
     `secret_id` (`receive_proposed` Memory-supersede pattern); calls
     stubs `self.vault_receipts_on_deposit(..)` (S3c) and
     `self.vault_grant_on_commit(..)` (S4) that exist as no-ops.
  5. Vault projection `vault_state()` in `vault/mod.rs`: deposits by
     `(depositor, name)` last-wins, pending deposits, grants with
     current/void; `vault_view()` fills the contract view (receipt and
     complaint fields from `vault/receipts.rs` helpers returning empty
     until S3c).
  6. Card state: `pending` (proposal open), `committed`; `sealed` /
     `hardened` come from S3c's counts.
- **Red tests first:**
  - unit `a_deposit_is_refused_outside_a_vault_republic` (v5 with `vault` feature too).
  - unit `approve_waits_for_the_payload` - approve refused with `payload not held`, then accepted once the file is held.
  - unit `approve_refuses_a_share_that_fails_feldman`.
  - unit `a_forged_depositor_is_refused_by_the_approver`.
  - verify unit `verify_chain_rejects_a_deposit_with_a_forged_signature`
    and `..._with_a_wrong_holder_order`.
  - unit `a_replace_drops_the_old_payload_and_supersedes_pending_grants`.
  - unit `generic_propose_on_vault_is_refused_in_a_vault_republic`.
  - unit `the_view_lists_name_kind_and_size_but_never_text`.
  - integration `tests/vault_deposit.rs`:
    `a_deposit_commits_only_once_the_approver_holds_the_payload`
    (2-of-4; the approving seat's fetch is held back until it has the
    file; KEYSTONE §14.3 a),
    `a_forged_depositor_is_refused_by_approver_and_verifier` (KEYSTONE §14.3 b).
- **Verify:** `cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_deposit --test vault_plane --test vault_founding && cargo test -j 2 -p molt-engine && cargo clippy -j 2 -p molt-engine --all-targets`.

## S3c - Receipts, complaints, reveal, re-seal, lab seam

- **Goal:** cards count what is proven; complaints are decided and named;
  the depositor is offered a re-seal.
- **Spec:** §7 (receipts, complaints, decision, UI table, D15), §10 bullets 2-3.
- **Owns:** `crates/molt-engine/src/vault/receipts.rs`, `vault/complaint.rs` (new),
  `crates/molt-engine/src/net/presence.rs` (one tick line),
  `crates/molt-engine/Cargo.toml` + `crates/molt-app/Cargo.toml`
  (`vault-lab` feature, off by default), `crates/molt-engine/tests/vault_complaints.rs`.
- **Steps:**
  1. A holder checks its share and the held payload whenever a deposit
     (pending or committed) and its payload are both present; result
     cached in runtime; sends `vrcpt` `verified` or `complaint` once per
     `secret_id`, re-sent by the presence tick on start (mirror-decl
     pattern), stored last-wins in `TransportState.vault_status` at every
     member (`persist` off-actor like `persist_mirror`).
  2. Depositor answers each complaint it sees with a `vrevl` from
     `rederive_share`; every member runs `molt_vault::decide` against the
     record and the holder's roster `vault_pk`; outcome stored.
  3. View: `verified` = holders other than the depositor with a
     `verified` receipt AND no open complaint; `sealed` at `>= m`,
     `hardened` at `n-1`; `readable_by = m - valid revealed shares`;
     complaint lines per spec §7 table (`bad share from <depositor>`,
     `false complaint by <holder>`, `complaint open`,
     `readable by <k> instead of <m>`).
  4. `cmd_vault_reseal`: depositor only; `recover_secret` + decrypt the
     held payload + `cmd_vault_seal` with the same name/kind (a vote like
     any deposit, D15: never automatic).
  5. Receipts and reveals never enter the vault base; at a cut, entries
     for `secret_id`s no longer current are pruned.
  6. `vault-lab` feature (dev only, never default, never in
     `scripts/build-release.sh`): when compiled in and
     `MOLT_VAULT_LAB_COMPLAIN=1` is set at spawn, this node's holder check
     reports `complaint` regardless of its result - the only way to
     exercise false complaint -> reveal -> re-seal by hand. A test
     asserts the feature is absent from the default graph.
- **Red tests first:**
  - unit `sealed_counts_holders_other_than_the_depositor`,
    `hardened_needs_all_n_minus_1`, `a_complaint_holds_the_card_below_sealed`.
  - unit `receipts_are_last_wins_per_holder_and_survive_a_restart`.
  - unit `a_receipt_from_a_non_holder_is_ignored`.
  - integration `tests/vault_complaints.rs` (2-of-4, test seams in
    `#[cfg(test)]`/the test harness for a dishonest depositor and a lying
    reveal): `a_false_complaint_names_the_complainer`,
    `a_bad_share_names_the_depositor`, `a_lying_reveal_names_the_depositor`
    (KEYSTONE §14.3 c), `a_reveal_lowers_readable_by_and_offers_reseal`,
    `a_reseal_is_a_vote_and_restores_the_threshold`.
  - `vault_lab_feature_is_off_by_default` (cargo tree / cfg check).
- **Verify:** `cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_complaints --test vault_deposit && cargo clippy -j 2 -p molt-engine --all-targets && cargo clippy -j 2 -p molt-engine --all-targets --features vault-lab`.

## S4 - Grant and read

- **Goal:** a grant binds one version, commits at m, holders answer on
  commit and on request, the reader combines any m valid shares and
  reads; everyone else sees an audit entry.
- **Spec:** §6 (Grant), §8, §10 bullets 5-8, §11, §14.4.
- **Owns:** `crates/molt-engine/src/vault/{grant.rs, read.rs}`,
  `crates/molt-engine/tests/vault_grant.rs` (new),
  `crates/molt-mcp/tests/vault_tools.rs` (new).
- **Steps:**
  1. `cmd_vault_grant { secret_id, reader }`: secret must be the current
     version; reader a seat; computes `grant_id` from the proposal id the
     propose path assigns; proposes `grant`.
  2. Approve on a grant: base-pending refuses; void target refuses.
  3. `vault_grant_on_commit`: if valid (current version at that height)
     and this seat is a holder, `seal_resp` its opened share to the
     reader with `resp_aad` and publish `vresp`; base-pending queues it.
  4. `vask` from the reader (MLS sender == reader of a committed, valid
     grant) -> answer again; idempotent, rate-limited per (grant, asker)
     to once per 10 s.
  5. Reader: answers collected in runtime memory only (never persisted),
     each checked against the commitments (bad -> seat named in the
     view as `bad answer from <seat>`); own share added if a holder.
     `cmd_vault_read`: `>= m` valid -> decrypt the held payload ->
     `Reply::VaultText` (never stored, never in `read_state`); else send
     `vask` and return `Reply::VaultPending { have, need }`; emit
     `Event::VaultReadable` when the m-th answer arrives.
  6. Seats stop answering grants whose version was replaced.
  7. View grants: `pending`/`committed`/`void`, `at` = local commit time
     when the log has it, `mine` = reader is me.
- **Red tests first:**
  - unit `grant_id_recomputes_or_the_grant_is_refused`.
  - unit `a_grant_on_a_replaced_version_is_void_and_unanswered`.
  - unit `an_answer_for_another_grant_does_not_open` (AAD).
  - unit `a_bad_answer_names_its_seat_and_the_read_still_succeeds_with_m_good`.
  - unit `the_plaintext_is_never_persisted` (log, transport.state, chain
    scanned for the text after a read).
  - integration `tests/vault_grant.rs`:
    `the_reader_decrypts_with_one_seat_dead` (2-of-4, one holder closed
    before the grant commits) - KEYSTONE;
    `a_non_reader_cannot_decrypt` (another seat's `vault_read` -> `not the reader`,
    and the answers it sees on the wire do not open with its key) - KEYSTONE;
    `a_reader_restored_from_a_pre_grant_backup_reads_by_asking_again` - KEYSTONE;
    `a_replace_racing_a_grant_supersedes_it` - KEYSTONE.
  - mcp `tests/vault_tools.rs`: `vault_tools_drive_seal_grant_read`
    (headless engine over the MCP surface) and
    `the_read_only_key_never_reaches_vault_read`.
- **Verify:** `cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_grant --test vault_deposit && cargo test -j 2 -p molt-mcp && cargo clippy -j 2 -p molt-engine --all-targets && cargo clippy -j 2 -p molt-mcp --all-targets`.

## S5 - Fold, cut, recovery, backup

- **Goal:** a cut folds the vault into a committed base held on the file
  plane; base-pending is typed; recovery and every-seat-restore keep the
  vault.
- **Spec:** §9.3, §9.4, §9.5, §14.5; plan 1.3.6.
- **Owns:** `crates/molt-core/src/chain.rs` (`CheckpointVault`,
  `molt-chain-checkpoint-v10` + pins, `approval_bytes` arm),
  `crates/molt-engine/src/chain/{vault_base.rs (new), checkpoint.rs, verify.rs, governance.rs}`,
  `crates/molt-engine/src/vault/fold.rs`, `crates/molt-engine/src/net/vault_base.rs` (new),
  `crates/molt-engine/src/{recovery.rs, session.rs, lifecycles.rs (recovery/restore seed derivation)}`,
  `crates/molt-engine/src/chain/membership.rs` (keep `vault_pk` across Membership),
  `crates/molt-storage/src/{lib.rs (vault_base.bin sink), export.rs, import.rs}`,
  `crates/molt-engine/tests/vault_recovery.rs` (new),
  `docs_archive/storage/backup_restore_design.md`, `docs_archive/memory/knowledge_base_scale.md` (one cross-reference each).
- **Steps:**
  1. Core: `ChainChange::CheckpointVault { upto, state_hash }`; approval
     bytes tag `molt-chain-checkpoint-vault-v1` (sibling of the folded arm);
     checkpoint canonical bytes v10 when `founding_identities` carry
     `vault_pk` (layout of 1.3.6).
  2. Fold: `vault_base.rs` builds `VaultBase` from the held base + the
     vault group (current deposits last-wins by authenticated depositor,
     grants on current versions only - D16), commitment over
     `vault_base_canonical_bytes`; the cut replaces the vault group by one
     `vault_base` entry. Propose, co-sign, verify and apply take the base
     held NOW as a parameter (the §4.9.5 rule; mirror `fold_cut`'s
     signature). Persist the base BEFORE dropping blocks.
  3. `ChainWalk`: in a v6-genesis chain refuse `Checkpoint` and
     `CheckpointFolded`; accept only `CheckpointVault`.
  4. Base plane: `vault_base.bin` sink (segment MAX-5), series family
     (`molt-vault-base-v1` key info), `vault_base_tick`, `serve`, adopt with
     hash check (mismatch -> delete and refetch, never refuse).
  5. Base-pending: `read_state(vault)` returns `base_pending: Some(progress)`
     and empty lists are never shown as "no deposits"; approve of deposits
     and grants refused with `VaultBasePending`; answers to committed
     grants queued until the base arrives; nothing retired against an
     empty base. Payloads of the current versions are re-fetched by the
     S3a tick once the base names them.
  6. Recovery / restore: derive `vault_seed` from the phrase + this seat's
     FOUNDING `nostr_pk` and `identity_pk` (from the chain), check it
     against the roster `vault_pk`, persist it. Membership changes keep
     `vault_pk`.
  7. Export / import: `vault_base.bin` and `vault/<secret_id>.bin` (64
     lowercase hex stem, no extra path parts) ship only if they
     authenticate; a failing file is named and left out; the import plants
     only authenticating files and lists the rest in `dropped` (the
     restore log line already names them). Open checks the commitments.
- **Red tests first:**
  - core `checkpoint_v10_byte_pin`, `checkpoint_v10_hashes_vault_pk_in_both_tables`
    (swapping one `vault_pk` changes the hash), existing v6-v9 pins untouched,
    `checkpoint_vault_approval_bytes_pin`.
  - engine unit `a_cut_folds_the_vault_to_one_base_entry`,
    `the_fold_keeps_only_the_current_version_and_its_grants` (D16),
    `the_fold_takes_the_base_held_now`, `a_legacy_checkpoint_in_a_vault_chain_is_refused`,
    `base_pending_is_a_typed_refusal_not_an_empty_list`,
    `a_tampered_vault_base_is_refetched`.
  - storage `a_vault_base_and_payloads_travel_with_the_backup`,
    `a_damaged_vault_file_is_named_and_the_rest_still_backs_up`,
    `a_foreign_vault_file_is_dropped_and_the_rest_restores`,
    `the_import_allowlist_takes_only_hex_vault_files`.
  - integration `tests/vault_recovery.rs`:
    `deposit_cut_recover_grant_to_the_recovered_seat_reads` - KEYSTONE §14.5;
    `a_vault_survives_every_seat_restoring_from_backup_after_a_cut` - KEYSTONE,
    twin of `a_folded_wiki_survives_every_seat_restoring_from_backup`.
- **Verify:** `cargo test -j 2 -p molt-core && cargo test -j 2 -p molt-storage && cargo test -j 2 -p molt-engine --lib && cargo test -j 2 -p molt-engine --test vault_recovery --test wiki_base_plane --test nostr_recovery --test restore_real --test checkpoint_under_load && cargo test -j 2 -p molt-engine && clippy core, storage, engine`.

## U1 - Wizard, charter echo, org lock, strings

- **Goal:** the founder can choose the vault, only inside the bounds; no
  path lets members vote the vault in later.
- **Spec:** §3, D11, D14 (UI side).
- **Owns:** `crates/molt-ui/src/{actions/ritual.rs, mirror.rs (feature echo lines), labels.rs, i18n.rs}`,
  `crates/molt-ui-window/ui/{app.slint (wizard features block, org features modal), parts.slint (CharterView), theme.slint (Strings)}`,
  `crates/molt-ui/src/tests/**` touching these.
- **Steps:**
  1. `in-out property <bool> cw-feat-vault`; the AppCheck enabled iff
     `2 <= cw-threshold <= cw-members - 2`; when disabled it shows
     `needs 2 <= m <= n-2`, and turning the threshold out of range
     unticks it. `on_create_propose` adds `(get_cw_feat_vault(), "vault")`.
  2. CharterView and the joiner echo show `vault` from the real feature
     list (founder echo no longer hard-codes false).
  3. Org features modal: `vault` is not votable (row shows `founding only`,
     no checkbox); `of-vault-draft` removed.
  4. `labels.rs default_op(Vault)` no longer offers `seal_secret`.
  5. Strings: `feat-vault-bounds`, `vault-founding-only` (plus any
     U2 strings may land here to save a .slint round-trip).
- **Red tests first:** GUI (live-preview)
  `the_vault_box_is_disabled_outside_2_to_n_minus_2` (2-of-3 off, 2-of-4 on, 3-of-4 off),
  `a_ticked_vault_reaches_create_propose`,
  `the_org_modal_cannot_vote_the_vault_in`, `the_charter_echo_shows_the_vault`;
  i18n parity test.
- **Verify:** `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test -j 2 -p molt-ui --lib --features molt-ui/live-preview`; live-preview clippy; `scripts/dev-ui.sh build`.

## U2 - Vault pane, real (fixture-fed)

- **Goal:** the VaultPane renders `VaultView` and issues the four vault
  commands; no mock literal remains.
- **Spec:** §1 (deposit warning line), §4 (replace warning), §7 (states
  and the card text table), §8 (grant, read, audit), D4, D8.
- **Owns:** `crates/molt-ui-window/ui/{surfaces.slint (VaultPane and its cards), app.slint (vault region, seal/grant/read modals)}`,
  `crates/molt-ui/src/{surfaces.rs (bundle + apply), actions/vault.rs (new), i18n.rs, labels.rs (card titles)}`,
  `crates/molt-ui-window/ui/theme.slint` (Strings), `crates/molt-ui/src/tests/gui/vault.rs` (new),
  `crates/molt-ui/src/tests/gui/layout.rs` (rewrite `the_vault_seal_button_opens_its_dialog`).
- **Steps:**
  1. Slint models: `VaultDepositRow`, `VaultGrantRow`, `VaultComplaintRow`
     replace `MockSecret`/`MockRequest`/`MockUnsealed`; `in property`
     models fed from Rust; in-place `sync_rows` patching (never swap a model).
  2. `gather_surfaces` keeps the Vault snapshot's `vault` in
     `SurfacesBundle`; `apply_surfaces` fills the models and
     `vault-real`, `vault-m`, `vault-n`, `vault-base-pending`.
  3. Views: `secrets` (cards: name, kind, depositor, size, state chip
     `pending`/`committed`/`sealed`/`hardened`, `verified/holders`,
     complaint lines from the §7 table, `readable by <k> instead of <m>`,
     own card `Re-seal` button when a valid reveal exists, `Grant` and
     `Read` actions), `requests` (grants: pending ones with vote, committed
     audit entries `name - reader - when`, void ones dimmed), `unsealed`
     (texts read in THIS session, held only in UI memory, cleared on
     workspace close).
  4. Seal modal: name, kind (presets `text`, `password`, `key`, `will`;
     free entry allowed), text with a byte counter `n / 102400`, refusal
     over the cap; the one-line warning
     `Any m members can open this at any time - by vote for one of them, or among themselves. There is no way back.`
     (m substituted); when the name exists for me: `Replaces <name>. Old copies stay readable in backups.`
     -> `VaultSeal`.
  5. Grant modal: reader picker over seats -> `VaultGrant`.
  6. Read: `VaultRead`; `VaultPending` shows `waiting for answers <have>/<need>`;
     `Event::VaultReadable` triggers the re-read; `VaultText` shows the
     text in `InsetWell` with copy.
  7. `real == false`: one line `needs a vault republic`, no actions.
     `base_pending`: `loading vault <have>/<size>`.
  8. Proposal cards for Vault (`summarize`/`display_title`): deposit
     `deposit <name> (<kind>)` / `replace <name> (<kind>)`, grant
     `grant <name> to <reader>` (D8).
- **Red tests first** (live-preview, fixture `VaultView` through
  `apply_surfaces`): `the_pane_shows_committed_sealed_hardened`,
  `each_complaint_status_renders_its_line`, `a_lowered_threshold_renders_readable_by`,
  `reseal_shows_only_on_the_depositors_card_after_a_reveal`,
  `the_seal_modal_refuses_over_100_kib`, `the_seal_modal_warns_on_replace`,
  `seal_confirm_issues_vault_seal`, `grant_confirm_issues_vault_grant`,
  `read_shows_pending_then_text`, `a_non_vault_republic_shows_one_line`,
  `no_em_dash_in_vault_strings`, `model_rows_patch_in_place` (ptr_eq).
- **Verify:** as U1.

## U3 - Real-engine GUI tests, gui_over_mcp, lab script

- **Goal:** the pane proven against a real engine; the user can bring up
  a 2-of-4 vault republic with one command.
- **Owns:** `crates/molt-ui/src/tests/gui/{mod.rs (vault workspace helper), vault.rs (real-engine tests)}`,
  `crates/molt-ui/src/mirror.rs` (`vault_rows` in `build_ui_snapshot`),
  `scripts/vault_lab.py` (new), `scripts/gui_walk.py` (optional vault step).
- **Steps:**
  1. Helper `vault_workspace_on_disk(root, m, roster)` (roster-v6 genesis
     with derived `vault_pk`s, `features: Some(["vault"])`).
  2. Real-engine GUI tests: deposit from the pane -> proposal card ->
     approve -> committed; grant -> read -> text in the unsealed view.
  3. `scripts/vault_lab.py` (stdlib only, patterns from `gui_walk.py`):
     `up --relay <url> [--seats 3] [--complainer s3]` writes
     `/tmp/vault-lab/{gui,s1,s2,s3}/config.toml` (one shared token, ports
     from `free_port`, relay confirmed, clearnet on), starts the headless
     seats from `target/dev-ui/debug/moltd` (`--features live-preview,vault-lab`
     build when `--complainer` is given, the seat launched with
     `MOLT_VAULT_LAB_COMPLAIN=1`), and prints the GUI command;
     `join` reads the founder's seat links from the GUI node's MCP port
     (`read_session`), joins every headless seat (retrying non-joinable
     preview links), confirms charter and seed backup;
     `approve` approves every pending proposal on every headless seat
     (retrying `payload not held` / `not verified`); `grant <name> <reader>`,
     `read <seat> <name>`, `stop <seat>`, `start <seat>`, `status`, `down`.
- **Red tests first:** `the_vault_pane_drives_a_real_engine_deposit`,
  `a_granted_read_shows_the_text_in_unsealed`,
  `ui_snapshot_counts_vault_rows`.
- **Verify:** molt-ui live-preview tests + clippy; `scripts/dev-ui.sh build`;
  a smoke run of `vault_lab.py up/join/approve/down` against a GUI-less
  founder (`--founder-headless`) asserting a committed deposit, printed
  in the stage report.

## S6 - Final gate and archive

- **Goal:** everything green on master; docs tell the truth.
- **Owns:** `CLAUDE.md` (roster-v6 / checkpoint-v10 / CheckpointVault in
  the conventions list; molt-vault in the crate order; vault spec in the
  read-first list), `docs_archive/ritual/charter_features.md` (vault is
  real, founding only), the spec (status: shipping spec, moved from
  `docs/vault/` to a new `docs_archive/vault/` directory), this plan
  (status: EXECUTED, moved beside it),
  `docs/reviews/known_debt.md` (V6 share refresh and anything deferred).
- **Steps:** run the gate list (section 4), a code review over the whole
  vault diff (`/code-review high` or equivalent agent pass) with every
  finding fixed, then the manual recipe once headless (lab with
  `--founder-headless`), then archive the docs and run the doc checker.

## 4. Final gate list (run on master, in this order)

1. `cargo test -j 2 -p molt-core`
2. `cargo test -j 2 -p molt-vault`
3. `cargo test -j 2 -p molt-storage`
4. `cargo test -j 2 -p molt-net` (incl. `ring_free_guard`, `frame_disjointness`)
5. `cargo test -j 2 -p molt-engine` (incl. `c_free_guard` and every `vault_*` integration test)
6. `cargo test -j 2 -p molt-mcp`
7. `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test -j 2 -p molt-ui --lib --features molt-ui/live-preview`
8. clippy 0: `cargo clippy -j 2 -p <c> --all-targets` for core, vault, storage, net, engine, mcp; engine again with `--features vault-lab`; molt-ui via the live-preview line.
9. `scripts/dev-ui.sh build`
10. `cargo tree --locked -p molt-vault -e no-dev -i ring` and `-p molt-net ... -i ring` empty; `cargo tree -d -p molt-vault` free of second copies of the curve/AEAD/hash stack.
11. `python3 scripts/check-doc-refs.py` exits 0.
12. Keystones (all must be in step 5's run): `a_vault_founding_round_trips_roster_v6`,
    `a_vaultless_founding_stays_byte_identical`, `verify_seal_proposal_rejects_a_tampered_own_vault_pk`,
    `verify_seal_proposal_rejects_a_duplicate_vault_pk`,
    `a_deposit_commits_only_once_the_approver_holds_the_payload`,
    `a_forged_depositor_is_refused_by_approver_and_verifier`,
    `a_false_complaint_names_the_complainer`, `a_bad_share_names_the_depositor`,
    `a_lying_reveal_names_the_depositor`, `the_reader_decrypts_with_one_seat_dead`,
    `a_non_reader_cannot_decrypt`, `a_reader_restored_from_a_pre_grant_backup_reads_by_asking_again`,
    `a_replace_racing_a_grant_supersedes_it`, `deposit_cut_recover_grant_to_the_recovered_seat_reads`,
    `a_vault_survives_every_seat_restoring_from_backup_after_a_cut`.

## 5. Manual test recipe (for the user)

Builds: `scripts/dev-ui.sh build` once (live-preview, light); for the
complaint path also
`CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo build -p molt-app --features live-preview,vault-lab`.

1. Relay: `cargo run -p molt-net --example dev_relay` - note the
   `ws://127.0.0.1:<port>` URL; leave it running.
2. Seats: `python3 scripts/vault_lab.py up --relay ws://127.0.0.1:<port> --seats 3 --complainer s3`
   - three headless seats `s1`..`s3` start; the script prints the GUI command.
3. GUI: `scripts/dev-ui.sh run --config /tmp/vault-lab/gui/config.toml`.
4. Found: Create republic, 4 members, threshold 2. The Vault box is
   enabled (set threshold 3: it greys out with `needs 2 <= m <= n-2`;
   set it back to 2). Tick Vault, propose the charter, then
   `python3 scripts/vault_lab.py join`; confirm your seed backup and
   finish in the GUI.
5. Deposit: Vault pane -> Seal -> name `test`, kind `text`, a few lines.
   Read the warning line. `python3 scripts/vault_lab.py approve` -> the
   card goes `pending` -> `committed` -> `sealed` (2 holders verified) ->
   `hardened` stays out of reach while s3 complains; the card shows
   `false complaint by s3` and `readable by 1 instead of 2`, and your card
   offers `Re-seal`.
6. Re-seal: press it, `vault_lab.py approve` -> the new version commits;
   the old card disappears, the threshold line returns to normal (s3
   complains again and is named again - expected with the lab seam).
7. Grant and read: Grant `test` to yourself, `vault_lab.py approve`,
   press Read -> `waiting for answers` then the text. `vault_lab.py stop s1`,
   grant again (to yourself or `s2`), approve with the remaining seat: the
   read still works with one seat dead. `vault_lab.py read s2 test`
   shows a non-reader is refused.
8. Bounds: a second founding at 2-of-3 cannot tick Vault; the org
   features modal never offers it.
9. `python3 scripts/vault_lab.py down`; stop the relay.

Without `--complainer` (plain `scripts/dev-ui.sh build` binaries) steps
5-6 show the honest path: `hardened` once all three seats verified, no
complaint lines, no Re-seal.

## 6. Risks

- **The spec deviation 1.3.1 (vault-key salt) changes a ratified formula.**
  It keeps every property the spec states and is the only one-round
  option; the alternative (an extra ritual round after the charter) costs
  new ritual messages on both legs. Flag to the user with S0; S2 builds on it.
- **vsss-rs 5.1.0 is the older major** (6.0.1, 2026-07-31, is current).
  Pinned `=5.1.0`; the dealing byte-pin catches any drift; an upgrade to 6
  waits until OpenMLS moves to `curve25519-dalek` 5.
- **Feature unification**: enabling `group`/`rand_core` on the shared
  `curve25519-dalek` 4.1.3 touches OpenMLS's copy; additive, but S1 runs
  the molt-net MLS tests once to confirm.
- **`hpke` RNG consumption** is the decidability linchpin; pinned by
  `ikm_rng_draws_exactly_one_ikm` and `=0.13.0`.
- **Proposal budget** at n = 13: 12 x 80-byte shares hex-encoded + 2 x
  32-byte commitments x m: ~3 KiB, far below `payload_fits`; S3b asserts
  the largest roster in a test.
- **4-seat relay tests are slower**: `vault_support::found_n_at` is shared
  and every vault integration test uses 2-of-4 only.
- **Two agents building engine-scale crates** in W5 (S3c, S4): both in
  molt-engine; separate target dirs double the build. `free -g` first;
  if under 8 GiB free, run them back to back instead.
- **Intermediate master states** (after S0, before U2) show the vault as
  implemented while the pane is still the mock; acceptable inside one
  build session, never a release.
