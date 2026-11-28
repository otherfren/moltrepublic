# Multisig wallet surface: implementation plan, stage 1

Status: **EXECUTED (Stage 1 built 2026-10-10).** The spec of the shipping
behaviour is `docs_archive/chain/wallet_treasury_design.md`; open items are
in `docs/reviews/known_debt.md`.

As of: **Revision 3, 2026-10-06**, anchors checked against master `0f0cb30d`.
Replaces revision 2 (same day); §17 lists the changes.
Design authority: `docs_archive/chain/wallet_treasury_design.md` (rev 3). This
document is the build plan: files, symbols, tests, order.

**Scope: stage 1 only** — set up the purse, receive, watch.
Spending (stage 2) waits for SA+L threshold signing in monero-wallet
(W2, design §8). Do not create any code for stage 2.

**Mandatory for the implementation:**
- Line numbers drift — **always grep by symbol name.**
- Do not invent an upstream API. The dependency spike (§6) locked the core APIs
  against the compiler; read everything else in
  `~/.cargo/registry/src/*/<crate>-<ver>/` before use.
- The repo rules from `CLAUDE.md` apply in full (§3).

---

## 1. Mission and decisions

A republic gets a shared Monero purse. All n members hold a key part from a
DKG; spending later uses the same threshold `rule_m` with which the republic
governs.

| | Decision (user, 2026-10-06) |
|---|---|
| W1 | Purse only at `2 ≤ m ≤ n − 1` |
| W2 | Stage 2 waits for SA+L; no CLSAG spend |
| W3 | Monero only |
| W4 | Only an explicit wallet act (`wallet_init` vote) creates a purse; no marker, `set_features` cannot add `wallet` |
| W5 | Key file damaged: export aborts; import without it, seat watch-only |
| W6 | One decline ends the run |
| W7 | DKG participant = position in the genesis founding table (vault order) |
| W8 | `wallet_created` carries n identity attestations; the projection checks them from the payload (survives any cut) |
| W9 | Purse on by default; the purse stage with a progress bar is the automatic last wizard step |
| W10 | Enabling it later shows the same stage; all n online at the same time |
| W11 | Mainnet allowed; next to the address, large: „Spending does not work yet. Money sent here is gone.“ |

Still valid: republic = wallet, one threshold notion, XMR only,
charter feature, can never be switched off.

## 2. The stack (verified by spike 2026-10-06)

Two spikes (not in the repo): `treasury-spike` (DKG, address, view key,
FROST signature) and `stage1-min` (stage 1 without `multisig` and without
`modular-frost`: DKG test green, no `ring`, no C). Re-checked the same day
against the crate sources.

**Stage 1 needs:**

| Crate | Version | Role |
|---|---|---|
| `dkg` | 0.6.1 | `ThresholdParams`, `ThresholdKeys`, `Participant` |
| `dkg-pedpop` | **=0.6.0** (pin) | DKG, 2 rounds, `BlameMachine`; **deleted on Serai's `next`**; reviewed, not vendored (design §12) |
| `dalek-ff-group` | 0.5 | `Ed25519` ciphersuite |
| `ciphersuite` | 0.4 | `Ciphersuite` trait |
| `monero-wallet` | 0.2.0, **without** `multisig` | `ViewPair`, `Scanner`, addresses |
| `monero-daemon-rpc` | 0.2.0 | RPC client over our own `HttpTransport` |
| `schnorr-signatures` | **=0.5.2** (pin) | 0.5.3 widens the `multiexp` range → `multiexp` 0.4 + 0.5 side by side → `dkg-pedpop` breaks (two `BatchVerifier`s) |

`modular-frost` and the `multisig` feature (outside SemVer) only come
with stage 2.

Verified API facts:
- DKG: `KeyGenMachine::new(params, context: [u8; 32])` →
  `generate_coefficients(rng)` → `generate_secret_shares(rng, map)` →
  `calculate_share(rng, map)` → `BlameMachine::complete()` / `.blame(..)`.
- All n must take part (`validate_map`). pedpop does **not** detect
  equivocation — „responsibility lies with the caller“; confirm completion
  with everyone (docs). → transcript hash (§7.4).
- The context should be unique per multisig. → run nonce in the context.
- `*::read` need the own `ThresholdParams`.
- Intermediate states are not serializable.
- `ciphersuite::read_G` only checks validity/canonicality, **not** the
  identity point → own check, for every point of both frames
  (design §12).
- **No view key from the DKG.** Address = `ViewPair::new(spend, view)
  .legacy_address(network)`; `ViewPair::new` takes monero-oxide's own
  `ed25519::Point` → conversion from dalek-ff-group needed; rejects torsion.
- **Not `GuaranteedViewPair`/`GuaranteedScanner`.** Standard `Scanner`;
  seen output keys MUST be checked and stored.
- `Scanner::scan` rejects blocks with `hardfork_version > 16`
  (`UnsupportedProtocol`); unknown output types fail at decoding.
- `monero-daemon-rpc`: `trait HttpTransport { post(route, body,
  response_size_limit) }`; auth is up to the implementer.
- Sizes (n=13): round 1 ≈ m·32 + 96 B, round 2 ≈ 128 B per recipient,
  `wallet_created` ≈ n·64 B signatures + fields.
- `monero-simple-request-rpc`: pulls in `ring` + `cc`. **Do not use.**

## 3. Repo rules that bite here in particular

1. **TDD.** Every step starts with red tests (§10).
2. **clippy = 0, tests included, per crate.** `.expect("…")`, never `.unwrap()`.
3. **Co-equality.** New `Command` variants: MCP tool (with `scope`) or
   `INTERNAL` (list in the test function
   `co_equality_every_command_is_a_tool_or_documented_internal`).
4. **Events additive**; no new rule in the chain verification (§7.7).
5. **No I/O in molt-core.** Handlers synchronous; blocking work off-actor.
6. **GUI:** `scripts/dev-ui.sh build` + live-preview tests. No full
   window build per change-set. Never `DISPLAY=:0`. No em dash in UI text.
7. **Directly on master;** review over the diff before landing.
8. **Never name secrets in tracked artifacts.**
9. **Keep status lines current;** after doc moves `scripts/check-doc-refs.py`.
10. **Founding ritual untouched** (I15): not a byte, not a step of
    `founding.rs`/`nostr_ritual.rs` changes; the purse stage begins
    after the seal.

## 4. Anchors in the repo

| Anchor | Location | Role |
|---|---|---|
| `Surface::Wallet`, views | `molt-core/src/lib.rs` (`Surface::views`) | stays; `send` stays a stub |
| `Surface::is_implemented` | `molt-core/src/lib.rs` (test ~l. 8380) | include Wallet |
| `effective_features` / `feature_on` / `require_feature` | `molt-engine/src/chain/projection.rs` | `wallet` effective also through an applied `wallet_init` |
| `propose_payload`, enable-only gate („already enabled“, „cannot be disabled“) | `molt-engine/src/proposals.rs` | `set_features` must not newly add `wallet`; `wallet_init` allowed without the feature |
| `cmd_propose` / `cmd_approve` (calls `require_feature`) | `proposals.rs` | wallet arm, approve checks; exempt `wallet_init` from the feature gate |
| `register_decline` (`veto_room`) | `proposals.rs` | `veto_room = 0` for `wallet_init` |
| `chain_sign_and_gossip_approval` | `chain/governance.rs` | the same checks here too |
| `try_commit` | `chain/governance.rs` | **unchanged** (seal at m) |
| `proposal_change` / `id_free_for` | `chain/governance.rs` | own ID for `wallet_created` |
| `rebase_pending_approvals`, `forget_votes_for` | `chain/governance.rs`, `events.rs` | re-sign; sibling cards |
| `own_approvals` (field of the chain state) | `molt-engine/src/lib.rs` | „own approval stands“ |
| `after_block_applied` | `molt-engine/src/chain/` | **one** hook for the auto-start; paths without it (checkpoint candidate `checkpoint.rs`, recovery adoption) start nothing |
| Create/join finish (after `materialize_workspace` in `lifecycles.rs`, `chain_blocks` = `None`) | `molt-engine/src/lifecycles.rs` | auto-init only here; never recovery, never reopen; ritual code itself unchanged |
| `own_signature_stands`, auto-co-sign of the checkpoint | `chain/governance.rs`, `chain/checkpoint.rs` | model for auto-co-sign |
| `vault_arm`, `vault_approve_check` | `molt-engine/src/vault/mod.rs` | model for a closed op set + check in every signing path |
| `supersede_stale_vault_cards` | `vault/deposit.rs` | model for closing sibling cards |
| `State::vault_founding_table()` | `molt-engine/src/vault/fold.rs` | **the** seat order (genesis/anchor, every roster); the wallet uses it directly |
| `bounds_ok` | `vault/mod.rs` | model for W1 |
| `CheckpointState.rule_m` / `.rule_n` / `.founding_identities` | `molt-core/src/chain.rs` | source for W1 and the attestation check; **not** `State::threshold()` (config fallback) |
| `net_scope_current`, `reset_workspace_state` | `net/mod.rs`, `events.rs` | lifetime of scanner/run |
| `cmd_net_presence_tick`, `vault::receipts::tick` | `net/presence.rs`, `vault/receipts.rs` | resend, deadlines |
| `CONTROL_FRAMES`, `ControlParser` | `molt-net/src/supervisor.rs` | new tags; frames are only MLS-authenticated |
| `vault_frames.rs`, `mirror_gossip.rs` | `molt-net/src/` | model for frames |
| `GroupHandle::publish_control` | `molt-net/src/group_runtime.rs` | best-effort (queue 64), hence resend |
| `TransportState` (`mirror`, `vault_status`) | `molt-core/src/lib.rs` | shareholder status, run state |
| Segments `−1 … −8` (`−7`/`−8` Kanban), `RESERVED_SEGMENT_FLOOR` (pinned by test) | `molt-storage/src/lib.rs` | new `−9` keys, `−10` scan; move floor + test along |
| `chain_state_key`/`hkdf32`, `encode_frame`, `write_atomic` | `molt-storage/src/lib.rs` | pattern for both files |
| `persist_chain_blocking`, `WriterMsg` | `molt-storage/src/lib.rs` | blocking writer |
| Export function with closure `checked_kind` | `molt-storage/src/export.rs` | export arms |
| `allowed_entry`, verify loop, `staging.dropped` | `molt-storage/src/import.rs` | import arms |
| Backup ticker | `molt-engine/src/backup.rs` | backoff on key error |
| `tools()`, `Scope` | `molt-mcp/src/lib.rs` | new tools (seat) |
| `VaultView` / `SurfaceSnapshot.vault` | `molt-core/src/vault.rs` | model for `WalletView` / `.wallet` |
| `classified` / `relay_kind` / `RelayKind` | `molt-core/src/relay.rs` | extract the host rule; today takes only `ws`/`wss` |
| `relay_block` / `clearnet_enabled` | `relay.rs`, `molt-config/src/lib.rs` | the same non-onion gate for the daemon |
| `s3::http` (free functions over a stream; `MAX_RESPONSE` module constant, `S3Error`) | `molt-net/src/s3/http.rs` | limit as a parameter, error type generic — refactor |
| `Dialer` (enum) | `molt-net/src/dial.rs` | transport |
| `Config` (`deny_unknown_fields`, `render`, `salvage`) | `molt-config/src/` | section `[wallet]` |
| Wizard feature step: `cw-feat-*`, wallet `checked: false; enabled: false` | `molt-ui-window/ui/app.slint` (~l. 3966) | default on, lock only at W1 |
| `SealedPanel` (founder `cw-outcome == 1`, joiner `jw-sealed`) | `app.slint`, `parts.slint` | afterwards automatically the purse stage |
| Organization modal `org-feat-wallet` (locked; ride-along `" wallet"`) | `app.slint` (~l. 6353, 9805) | checkbox → `wallet_init` instead of `set_features` |
| `WalletPane` (mock) | `molt-ui-window/ui/surfaces.slint`; route `app.slint` | becomes real |
| `default_op` (`"transfer"`), `wl_*` | `molt-ui/src/labels.rs`, `i18n.rs` | clean up |
| E2E harness | `molt-engine/tests/vault_support/mod.rs` | template |
| **Naming trap** `WalletHandle` | `molt-engine/src/lib.rs` | engine actor handle, not the purse |

## 5. Architecture

### 5.1 Crate `crates/molt-treasury`

Modelled on `molt-vault`: pure functions, no I/O, depends only on `molt-core`
and the crates from §2. Add to the workspace header.

```
crates/molt-treasury/src/
  lib.rs      // facade, TreasuryError
  dkg.rs      // rounds: context (with run nonce), identity check, transcript
  attest.rs   // attestation bytes, signing, verifying all n
  keys.rs     // view key from contributions, standard address, (de)serialization,
              // zeroize; byte layouts with tags + byte-pin tests
  scan.rs     // scanner core behind its own trait: ViewPair, seen keys,
              // block hashes, UnsupportedProtocol -> pause
```

- **Daemon URL:** in `molt-core/src/relay.rs` factor the host classification
  out of `classified` as a shared function (WHATWG parse, ASCII,
  no backslash, no userinfo, canonical spelling); entry point
  `daemon_kind(url)` for `http`/`https`, `relay_kind` unchanged for
  `ws`/`wss`. No second parser.
- **RPC transport** in molt-net (`monero_rpc.rs`): `HttpTransport` over the
  `s3::http` functions and the `Dialer`. Onion → Tor; local/clearnet →
  only with `clearnet_enabled` and a confirmed daemon (like a relay),
  otherwise fail-closed with a reason. Login: `http-auth` (digest-scheme), verdict
  in §6. Response limit per call (`get_blocks.bin` is large).
- **Seat order:** `State::vault_founding_table()` — no new helper.

### 5.2 Contract in molt-core (`molt-core/src/wallet.rs`, new)

```rust
pub struct WalletView {
    pub address: String,             // "" until the terminal block
    pub network: String,             // mainnet | stagenet | testnet
    pub balance: u64,                // >= 20 confirmations, piconero
    pub pending: u64,
    pub scan_height: u64,
    pub daemon_height: u64,
    pub connected: bool,
    pub scan_paused: Option<String>, // "update needed" (Fork) | "daemon fault"
    pub threshold: u32,
    pub participants: u32,
    pub phase: WalletPhase,          // Off | Bounds | NoPurse | Init | Ready
    pub run: Option<WalletRunView>,  // progress (§7.5)
    pub shareholders: Vec<(String, ShareStatus)>, // Held | WatchOnly | Unknown
    pub history: Vec<WalletTxView>,
    pub can_spend: bool,             // stage 1: always false
    pub can_watch: bool,             // the view key is here (7b)
    pub founding: bool,              // founding session with wallet: purse stage (7b)
}

pub struct WalletRunView {
    pub stage: RunStage,             // Ready | Round1 | Round2 | Attest | Sealing | Done | Aborted
    pub done: u32,                   // k of n
    pub of: u32,
    pub missing: Vec<String>,        // who is missing (readiness)
    pub reason: Option<String>,      // abort reason, one word/one line
    pub needs_consent: bool,         // this seat still has to consent
}
```

## 6. Dependency lock (step 2)

```toml
dkg = { version = "0.6", default-features = false, features = ["std"] }
dkg-pedpop = "=0.6.0"
dalek-ff-group = { version = "0.5", default-features = false, features = ["std"] }
ciphersuite = { version = "0.4", default-features = false, features = ["std"] }
monero-wallet = { version = "0.2", default-features = false, features = ["std", "compile-time-generators"] }
monero-daemon-rpc = { version = "0.2", default-features = false, features = ["std"] }
schnorr-signatures = { version = "=0.5.2", default-features = false }
```

1. In `[workspace.dependencies]`, keep the minor exact. Justify the pin in one
   line (multiexp split) so nobody „tidies it up“.
2. Bare `molt-treasury` with the spike functions. Compiling = locked.
3. Audit protocol: `cargo tree -d`, `-i ring`, `-i cc` empty, no `*-sys`.
4. **Review `dkg-pedpop`** (transcript, context, blame) and decide:
   crates.io pin or vendor (it is deleted on Serai's `next`,
   `dkg-evrf` unpublished). Verdict into design §12.
5. Search for and evaluate a digest-auth crate (§5.1).
6. Search for and evaluate a QR crate (§11).

**Done 2026-10-09** (verdicts with their arguments in Design §12):

- Locked in the workspace manifest; `molt-treasury` carries the spike
  functions (`dkg::{params, round1, round2, complete}`,
  `keys::standard_address`) and `tests/stage1_lock.rs` (3-seat DKG agrees,
  `ThresholdKeys` round-trip, standard main address; a round-1 frame under
  another context is refused; the `HttpTransport` shape compiles).
  `monero-daemon-rpc` is a dev-dependency there until molt-net's transport
  (§5.1) takes it; `molt-core` joins when first used (step 3).
- Audit: `cargo tree -p molt-treasury -d` shows only `thiserror` 1 + 2;
  `-i ring` and `-i cc` match nothing; no `*-sys`.
  `tests/graph_guard.rs` keeps that true and fails on a second `multiexp`
  or a moved `schnorr-signatures`. The pin only bites on a fresh resolve:
  an existing lock keeps `multiexp` 0.4 even at 0.5.3, the guard then
  goes red on the version.
- `dkg-pedpop`: crates.io pin `=0.6.0`, not vendored. Step 3 rejects the
  identity in every point of both frames, not only the commitments, and
  never calls `blame` with wire indexes.
- Digest auth: `http-auth` 0.1 with `default-features = false,
  features = ["digest-scheme"]`. Left: `digest_auth`.
- QR: `qrcode` 0.14 with `default-features = false` (no deps), module grid
  via `to_colors()`/`width()`. Left: `qrcodegen`, `fast_qr`.

## 7. Engine

### 7.1 The door (W1, W4)

- `Command::WalletInit` (tool, seat): gates W1 (from genesis/anchor),
  chain-governed, no applied or open `wallet_init`;
  daemon probe off-actor; proposal `{op: wallet_init, birthday_height,
  network}` on `Surface::Wallet`, birthday = daemon height − margin.
  `require_feature(Wallet)` does not apply to this op — at propose,
  `cmd_approve` **and** in the re-sign path.
- Navigation: wallet visible as soon as an init is open or applied.
- `effective_features` contains `wallet` as soon as a `wallet_init` is
  applied (from the wallet list of the fold, cut-proof).
- `set_features`: `wallet` only as a ride-along of an already effective
  feature; newly adding it → `BadPayload("wallet: set up the purse")`.
- Projection: the first `wallet_init` counts, every further one is ignored.

### 7.2 Init vote and consent (W6)

- **Approve checks** in `cmd_approve` **and**
  `chain_sign_and_gossip_approval` (model `vault_approve_check`):
  closed op set; birthday ≤ own daemon height + slack and
  not more than a fixed window below it; network = own; gates from
  §7.1. **Without a daemon: abstention** (neither approve nor decline);
  auto-approve as soon as one is set.
- **A decline kills the card** where it arrives: `veto_room = 0` for
  `wallet_init`. m approvals collected earlier still seal — the
  guarantee of W6 is the consent in the run.
- Seal at **m** (normal `try_commit`).
- A seat's consent = its own approve signature, as long as its own
  log carries it (`own_approvals` is built from the log), otherwise
  `WalletConsent` in the run.

### 7.3 Auto-init and wizard (W9)

- After the create/join finish (never recovery, never reopen): if the
  charter contains `wallet` and W1 holds, the lowest founding position
  **with a daemon** proposes the init; the next one after a delay, if
  no init is visible.
- A seat that ratified `wallet` **in this founding session**
  (ritual state, memory only) approves automatically after the
  birthday check.
- After the seal the wizard (founder and joiner) goes automatically into the
  purse stage: first the daemon (if none), then `WalletView.run`.
  „Enter republic“ is there from the start of the stage; leaving moves the
  bar into the panel. Without `wallet` in the charter the wizard ends at the
  seal as today.

### 7.4 Run and rounds (control frames)

Tags in `CONTROL_FRAMES`, each with `init_id` and run nonce `r`; resend at the
presence tick until commit or abort; never in the workspace log:

| Tag | Content |
|---|---|
| `\x00molt-wstart-v1` | start of a run: `init_id`, `r`, starter position |
| `\x00molt-wdhint-v1` | daemon URL as a suggestion, while a purse stage is open |
| `\x00molt-wrdy-v1` | build + daemon + consent, or decline |
| `\x00molt-wr1-v1` | commitments + PoP, view contribution `c_i` |
| `\x00molt-wr2-v1` | shares (per recipient) + transcript hash `T` |
| `\x00molt-watt-v1` | attestation `A_i` |
| `\x00molt-wabrt-v1` | abort with reason (no name) |

- **Starter:** founding position 1, fallback by position; starts
  automatically **once per init**, for the projection's first init, in the
  hook `after_block_applied` with an in-memory set of started inits
  (never at the restore on open); afterwards only via `WalletRetry` (tool,
  seat).
- **Races:** known nonce → refuse; two starts in readiness →
  everyone takes the one with the lowest starter position, then lowest nonce.
- **A seat with a keys record of an older run** joins new runs and
  keeps the record; should the old one complete after all, it wins.
- **Context:** `H("molt-wallet-dkg-v1" ‖ republic_id ‖ init_id ‖ r ‖ t ‖ n)`.
- **Transcript:** `T = H("molt-wallet-transcript-v1" ‖ r ‖ all round-1
  messages in participant order)`. A differing `T` → abort.
- **Equivocation / blame:** abort with a reason; sender and library blame
  only in the local log (`wallet_abort init=… run=… reason=… from=…`).
- Identity points (every point in rounds 1 and 2, group key) →
  abort before the library call.
- Ingest idempotent; ignore frames for a foreign/old run.
- `State.wallet_run` in memory. Deadline in the presence tick. Reopen without
  run state **and** without a keys record → abort frame; with a keys record →
  never abort, attest again.
- Encodings (`i` u16 LE, `init_id` u64 LE, `republic_id` raw 32 B, `r`
  32 B) with byte-pin tests.

### 7.5 Progress (W9, W10)

- `WalletRunView` from the run state: `Ready k/n` (with `missing`) →
  `Round1 k/n` → `Round2 k/n` → `Attest k/n` → `Sealing` → `Done`;
  `Aborted` with `reason`.
- Frontend `Event::WalletRunProgress` on every change, so that wizard,
  panel and MCP see the same state.
- Enabled later: every seat gets the panel over the main window at the
  start (§11).

### 7.6 Completion, attestation, terminal block (W8)

1. Locally: `ThresholdKeys`, view key, address.
2. Sign `A_i` (identity key, tag `molt-wallet-attest-v1`,
   fields design §3.5).
3. Write the keys record (share, view key, init, `r`, `T`, address, network, m, n,
   birthday, `A_i`) through the blocking writer (I4), **then**
   send `A_i`.
4. Whoever holds n valid attestations proposes `wallet_created`
   (position 1 first, then the next after a delay); own
   proposal ID; `attestations` in participant order.
5. Approvers co-sign only on an exact match **and** n valid
   attestations. Seal at m.
6. **Projection:** purse = the first `wallet_created` in the wallet list
   that points at the applied init, holds W1, carries the init's birthday and
   network, `threshold == rule_m`,
   `participants == n` (genesis/anchor) and carries n attestations that each
   verify under the `identity_pk` of founding position `i`
   (`vault_founding_table`). Payload + `CheckpointState` only.
7. At commit close sibling cards (model
   `supersede_stale_vault_cards`); delete the keys records of all other runs
   only when the purse block is final (below a
   checkpoint cut, where no reorg reaches).
8. **Restart:** keys records present, no purse → resend every attestation,
   co-sign the matching open `wallet_created`.

### 7.7 No new verification rule

- No rejection in `verify_chain` / `verify_next` / `fold_one`;
  checkpoint layouts unchanged.
- Op set at propose, approve, wire ingest and in every signing path.
- The projection deterministically ignores: other wallet ops, every
  `wallet_init` except the first, every `wallet_created` except the first
  valid one.

### 7.8 Recovery and status

- **Status frame** `\x00molt-wstat-v1` (Held/WatchOnly), per member in
  `TransportState`, resend at the presence tick.
- **Question/answer** `\x00molt-wvask-v1` / `\x00molt-wvresp-v1`: a seat without a
  view key asks after the rejoin; check `view·G` against the address.

### 7.9 Scanner

- Off-actor task per open workspace with its own generation (stale
  replies are dropped), aborted in `reset_workspace_state`.
- From `max(birthday, cursor)`; reply via a `Net*` command.
- Seen output keys; last block hashes per height for reorg detection.
- 20 confirmations; reorg → rewind or rescan from birthday.
- `UnsupportedProtocol` → `scan_paused = "update needed"`; decode error →
  `scan_paused = "daemon fault"`, never the pause.

## 8. Contract extensions

- `Command`: tools (seat) `WalletInit`, `WalletConsent { accept }`,
  `WalletRetry`, `WalletAcknowledgeLoss`; INTERNAL `NetWalletProbe`,
  `NetWalletFrame`, `NetWalletScan`, `NetWalletStatus`,
  `NetWalletViewAnswer` (names final at build time; list in the test function).
- No new `WorkspaceEvent` variants (runs are control frames).
- `Event` (frontend): `WalletRunProgress`, `WalletCreated { address }`,
  `WalletScanPaused`.
- `SurfaceSnapshot.wallet: Option<WalletView>`.
- MCP: the tools above with `Scope::Seat`. `read_state(wallet)` stays seat; the
  read key does **not** see the purse.
- `[wallet]`: `daemon_url`, `daemon_confirmed`, `daemon_login`,
  `network` (default `mainnet`, W11). In `Config`, `Settings`, `salvage`, `render`. No
  default daemon. Written only when different from the default: older binaries
  refuse only a config with a daemon set — release notes.

## 9. Persistence

| File | Segment | Content | Export | Import |
|---|---|---|---|---|
| `wallet_keys.state` | `u64::MAX − 9` | keys records (one per attested run, from the final purse on only the purse's) | check; damaged → **abort** | check; damaged → discard, watch-only; record ≠ purse → watch-only |
| `wallet_scan.state` | `u64::MAX − 10` | address, cursor, block hashes + times, seen keys, outputs | check; damaged → skip + name | check; damaged → discard, rescan |

- Sub-keys `hkdf32(ws_key, "molt-wallet-keys", id)` /
  `"molt-wallet-scan"`; `encode_frame` + `write_atomic`; cap
  `READ_CAP_STATE`. `RESERVED_SEGMENT_FLOOR` to `−10`, pin test along.
- Key file: `WriterMsg::PersistWalletKeys` +
  `persist_wallet_keys_blocking`. Records are appended, never replaced;
  only the final purse clears the others (I16).
- On open and import: record against the projected purse (init, run,
  address); deviation → watch-only.
- **Way out (W5):** `WalletAcknowledgeLoss` sets a damaged
  key file aside as a dot file (export ignores dot files);
  seat watch-only; exports run again (lifts `backup_hold`, test).
- **Backup ticker:** backoff on key error; one notice.
- Import with replace: the records of the replaced folder join those
  of the blob (I12); a damaged file there travels along if the blob brings
  none.
- Update `docs_archive/storage/backup_restore_design.md` §3.2 + allowlist.
  Release notes: older builds export without the key file
  and reject blobs that have it.

## 10. TDD roadmap (red tests first)

**molt-treasury:**
1. `a_three_seat_dkg_agrees_on_key_view_and_address` (byte round trip).
2. `the_context_binds_republic_init_and_run`.
3. `view_key_depends_on_every_contribution`.
4. `a_differing_transcript_aborts`.
5. `two_round_one_frames_from_one_sender_abort`.
6. `an_identity_point_in_either_frame_aborts` (every point of both
   frames; the group key: `an_identity_group_key_has_no_address`).
7. `the_address_is_a_standard_main_address`.
8. `attestations_verify_only_for_their_seat_and_fields`.
9. `an_attestation_from_another_run_or_init_fails`.
10. `wallet_keys_round_trip_and_zeroize`.
11. `a_repeated_output_key_is_ignored`.
12. `unsupported_protocol_pauses_and_a_decode_error_does_not`.
13. Byte-pin tests of all layouts (context, view, transcript, attestation);
    plus `attest_tag_cannot_collide_with_openmls_sign_content`.

**molt-core:**
14. `daemon_kind_classifies_onion_local_clearnet` (incl. userinfo,
    backslash, `ws://` rejected; `relay_kind` unchanged).

**molt-storage:**
15. `wallet_keys_and_scan_round_trip_under_their_segments`.
16. `a_damaged_keys_file_fails_the_export`.
17. `a_damaged_keys_file_restores_watch_only`.
18. `a_damaged_scan_file_is_skipped_and_named`.
19. `an_acknowledged_loss_lets_the_export_resume`.

**molt-engine (E2E over MockRelay, harness `vault_support`):**
20. `wallet_needs_two_to_n_minus_one`.
21. `set_features_cannot_add_wallet`.
22. `a_memory_vote_in_a_legacy_wallet_republic_creates_no_purse`.
23. `wallet_init_turns_the_feature_on`.
24. `one_decline_kills_the_init_card`.
25. `a_future_birthday_is_declined_but_a_lagging_daemon_is_not`.
25a. `a_seat_without_a_daemon_abstains`.
25b. `the_init_card_is_approvable_while_wallet_is_off`.
26. `a_seat_without_a_daemon_aborts_at_readiness_and_is_named`.
27. `a_declined_consent_aborts_the_run_and_retry_starts_a_new_nonce`.
28. `three_seats_found_one_purse` (same address, file before attestation).
29. `a_founding_with_wallet_runs_the_purse_stage` (auto-init, auto-approve).
30. `an_aborted_founding_stage_leaves_the_republic_founded`.
31. `a_late_enable_runs_the_stage_on_every_seat`.
32. `wallet_created_without_n_attestations_is_ignored`.
33. `m_seats_cannot_seal_a_purse_of_their_own`.
34. `an_anchor_rejoiner_computes_the_same_purse` (cut between init and
    `wallet_created` and after it).
35. `racing_wallet_created_cards_leave_one_purse`.
36. `a_mock_transfer_block_does_not_break_the_chain` (legacy republic).
37. `reopening_does_not_restart_a_run`.
38. `a_withheld_attestation_cannot_seal_a_run_without_shares`
    (records stay; whichever run wins, every seat holds its part).
38a. `racing_starts_converge_on_one_run` and `a_reused_nonce_is_refused`.
38b. `a_recovery_never_auto_proposes_an_init`.
38c. `a_restored_record_of_another_run_is_view_only`.
38d. `a_purse_with_a_foreign_birthday_or_network_is_ignored`.
39. `a_restart_after_persist_re_attests_and_cosigns`.
40. `a_phrase_only_recovery_is_watch_only_and_gets_the_view_key`.
41. `a_backup_restore_keeps_the_share`.
42. `the_read_key_does_not_see_the_wallet`.
43. Co-equality green; `is_implemented` test adjusted.

**Manual, `#[ignore]`:** regtest monerod behind `MOLT_TEST_MONEROD`
(`network = testnet` or regtest): mining visible; close/reopen keeps
state.

## 11. UI and usability (stage 1)

**Yardstick: the official Monero GUI client in Simple Mode.** Whoever can
operate it operates the purse without explanation. The multi-party machinery
stays invisible; only balance, receive, history are visible.

**Rules (review criterion, like compact texts):**

- **U1 No crypto vocabulary.** Never „DKG“, „round“, „transcript“,
  „attestation“, „share“, „view key“, „init“, „nonce“ in the UI. Words:
  *purse*, *set up*, *key part*, *view only*.
- **U2 One bar, four plain-language stages** (mapping of `RunStage`):
  `Ready` → „Waiting for members 3/5“ (+ who is missing); `Round1`/`Round2` →
  „Creating the purse“; `Attest` → „Confirming 4/5“; `Sealing` →
  „Sealing“; `Done` → address + warning. `Aborted` → one reason
  („Bob is offline“, „Bob declined“) and „Try again“.
- **U3 Amounts in XMR**, trailing zeros dropped, never piconero; grouping
  by locale.
- **U4 No inputs where none are needed.** Founding: the checkbox is the
  consent; the purse stage asks for the daemon only if none is
  set. Later: agree/decline, nothing else.
- **U5 Errors in one line**, the way out once.
- **U6 Technique only under „Details“** (collapsed): daemon, height,
  network, „Key parts held 4/5“, per member *key part* / *view only*.

**Views** (existing views `balance`, `receive`, `history`,
`status`):

| View | Content |
|---|---|
| Balance | large number „1.25 XMR“; below it „0.5 XMR pending“ only if > 0; without a purse a button „Set up the purse“ |
| Receive | warning (W11) large directly above the address; address monospace; „Copy“; QR code as a `monero:<address>` URI |
| History | per row: direction, amount, date, „3/20“ until confirmed, then a check mark; txid only in the context menu „Copy txid“ |
| Details (`status`) | U6 |
| Send | locked, one line „Spending is not available yet.“ |

**Daemon like the GUI's „Remote node“:** one field, one button. The starter
seat with a daemon announces its URL as a **suggestion** (`wdhint`,
ephemeral, never on the chain) while a purse stage is open; the
stage shows it prefilled with „Use“. Non-onion → the same confirmation dialog as for relays, once.
No bundled daemon. Whoever sets their own strengthens the
cross-check (design §7); the suggestion trades some of that for one
click.

**Wizard:**
- Feature step: wallet checkbox **on by default** when W1 holds;
  otherwise locked with a one-line reason. Below it the three lines from
  design §3.3 (worded U1-compliant).
- **Last step (founder and joiner):** after `SealedPanel`
  automatically the purse stage (U2, U4): first „Choose a node“ if
  none is set, then the bar; „Enter republic“ always there, the bar
  keeps running in the panel.

**Enabling later:** the same component as a modal over the
main window on every seat; agree/decline at `needs_consent`; one
line „Everyone must be online.“; „Try again“ after an abort.
Organization modal: wallet checkbox unlocked (W1), creates `wallet_init`.

**Otherwise:** loss dialog for the key file (§9); remove `default_op`
(`transfer`) and dead `wl_*`; no em dash. Tests go to
`tests/gui/vault.rs`, among them `the_warning_sits_beside_the_address`,
`the_wizard_ends_with_the_purse_stage`, `no_crypto_words_in_wallet_strings`
(word list U1 against all `wl_*`/wallet strings), `amounts_render_in_xmr`.

**QR code:** no own encoding: `qrcode` 0.14 without default features
(verdict §6).

## 12. Fork provision (FCMP++ + Carrot)

- Scanner behind its own trait; Carrot update = swap the implementation.
- `UnsupportedProtocol` → pause, balance stays.
- At the fork check: a Carrot-capable monero-wallet release; legacy scan with
  shared view key unchanged?
- `ThresholdKeys` and address stay; nothing to migrate.

## 13. Stage 2 — conditions, no build

Only when all three hold (design §8): `SalLegacyAlgorithm` over
`ThresholdKeys<Ed25519>` in a published monero-wallet (not
`SalAlgorithm`, which runs over `Ed25519T`); proof or audit;
fork heights fixed. Until then `can_spend = false`, no `sign.rs`.

## 14. Execution order

1. [x] Design rev 3 + plan rev 3 — for ratification.
2. [x] §6 dependency lock + bare `molt-treasury` + pedpop review +
   digest-auth verdict (2026-10-09).
3. [x] §10.1–13 molt-treasury; §10.14 daemon classification (2026-10-09).
   Round-1 message = `c_i ‖ pedpop bytes`, `T` covers both; `m`/`n` in
   attestation and keys record u16 LE, network one byte (0 main, 1 test,
   2 stage). `http://` only to onion/local (like `ws://`). The graph guard
   leaves out molt-core's subtree (chrono's platform crates). The
   scan file bytes come with step 4.
4. [x] §9 + §10.15–19 storage, backup docs (2026-10-09). Storage keeps
   the records opaque (no molt-treasury dependency, as with the vault):
   plaintext `molt-wallet-keys-file-v1` ‖ count ‖ length-prefixed records,
   byte-pinned. Append skips a byte-equal record and refuses a damaged
   file; `prune_wallet_keys(keep)` keeps the byte-equal record and refuses
   when it is not held. Scan bytes: `ScanState::encode/decode`
   (`molt-wallet-scan-v1`, seen keys = output keys). Set aside as
   `.wallet_keys.state.lost<n>`, never an intact file. The ticker's hold
   rides `NetBackupFailed.hold`: 1 h, doubling to 24 h, one notice. The
   record-vs-purse check stays with step 7 (`read_wallet_keys`,
   `ImportStaging::wallet_keys_dropped`).
5. [x] §8 contract, MCP, config; co-equality green (2026-10-09).
   `molt-core/src/wallet.rs` (view, `WalletRefusal`, `bounds_ok`);
   `SurfaceSnapshot.wallet` is boxed (clippy: `Reply` size). INTERNAL:
   `NetWalletProbe`, `NetWalletFrame`, `NetWalletScan`, `NetWalletStatus`,
   `NetWalletViewAnswer`. Session keys `wallet_daemon_url`,
   `wallet_daemon_confirmed`, `wallet_daemon_login` (write-only, stripped
   for the read key with any `wallet` snapshot), `wallet_network`. The
   daemon's one door is `patch_settings`: a wholesale save keeps it, a new
   URL starts unconfirmed. `[wallet]` is written only off its defaults, so
   older builds keep opening an untouched file; a URL or login the loader
   refuses never reaches the settings, and a save leaves its line alone.
   `WalletAcknowledgeLoss` acts on the open workspace, refuses while a
   backup is in flight, and lifts its backup hold. The phase is
   `Off`/`Bounds`/`NoPurse` from the genesis or anchor rule; the other
   doors answer `purse: not available yet` (`TODO(step N)` in
   `molt-engine/src/wallet.rs`).
6. [x] §7.1–7.2 + §7.7; §10.20–25, 36 (2026-10-10). The daemon
   transport came forward from step 8 (`molt-net/src/monero_rpc.rs`,
   `s3::http` with a per-call cap and its own `HttpError`; the
   `stub-daemon` feature is the in-process monerod of the tests).
   `WalletInit` probes off the actor and the probe's
   `NetWalletProbe` answers the caller with the proposal. Birthday =
   height − 10; an approver takes it up to 30 above and 1440 below its
   own daemon (a height older than 120 s is asked again). The
   projection counts the first *well-formed* applied init. A raw
   `propose` on Wallet answers `use wallet_init` (as the vault's). A seat
   judges each init card when it lands: another network or a birthday
   out of its window declines at once (the card dies, W6), also after an
   own approve whose re-sign aged out; a seat without a daemon, or whose
   daemon is still syncing (`get_info`), abstains on any network. An
   `approve` it cannot check yet is kept in memory, answers `... -
   approval held` and signs once the daemon answers. A malformed init is
   dropped at the wire; a close or a daemon change cancels a waiting
   `WalletInit` (`set-up cancelled`). `wallet_created` is refused at every
   signing path until step 7. `wallet` is no keep requirement of
   `set_features` (the union keeps it, like the vault), so the
   organization dialog no longer rides it along. §10.20–22 and 36 are unit tests on built
   chains (`molt-engine/src/wallet_tests.rs`); 23–25a also run end to
   end (`tests/wallet_init.rs`).
7. §7.3–7.6 + §7.8; §10.26–35, 37–41, split in two:
   - [x] 7a: §7.4–7.6; §10.26–28, 32–35, 37–39 (2026-10-10). The run
     lives in `molt-engine/src/wallet_run.rs`, the seven frames in
     `molt-net/src/wallet_frames.rs` (JSON, hex fields, 64 KiB cap);
     `NetWalletFrame.body` is the whole frame, tag included. Deadlines,
     fallbacks and resends ride the 1 s delivery beat, resends every
     10 s (the 30 s presence tick is too coarse for a readiness
     deadline). A seat that lost a run (reopen) learns its nonce only
     from the next round frame and answers it with `wabrt` `restart`;
     readiness frames of an unknown run wait as early frames, so a
     reopened seat may rejoin a run still in readiness (nothing of it
     was secret); the starter stops resending its start once it is in
     the rounds. A seat that left a run in readiness rejoins it on
     that run's round frame: all n readied it. `WalletRetry` refuses
     while this seat is in readiness or the rounds, a start only in
     the rounds. A decline is answered to every later frame of its
     run, so a lost one still arrives as a decline. The purse RNG
     fails closed: no nonce, and the run aborts. A decline withdraws
     this seat's consent for the init, its approval included, until it
     accepts again (in memory: after a reopen the approval counts again).
     `not ready` and `timeout` name the missing seats, a decline its
     decliner (§11 U2); no other abort names a seat. The other runs'
     records go once the purse block is below a cut, not at its commit:
     a reorg may still make another run the purse.
     The starter sends `wdhint` with its start; `WalletRunView` gains
     `daemon_hint`. New refusals: `no set-up running`, `set-up running`,
     `not this seat's result`, `no randomness`. `shareholders` names only this seat's
     own status until the status frame (7b). §10.32–35 and 38c/d are
     unit tests on built chains (`wallet_run_tests.rs`; 34 cuts the
     chain between init and purse, and after it); the rest run end to
     end (`tests/wallet_run.rs`) behind the test seams
     `__wallet_deadline`, `__wallet_withhold_attestation`,
     `__wallet_start_with`.
   - [x] 7b: §7.3 + §7.8; §10.29–31, 38b, 40–41 (2026-10-10). This
     seat's standing lives in `molt-engine/src/wallet_seat.rs`. A create
     or join finish with `wallet` ratified and W1 holding arms the stage
     (memory only; never a recovery or reopen): position k proposes
     `k-1` steps after arming once it has a daemon, until an init card
     or applied init is visible; the ratification is this seat's consent
     for that init's card (signed after the birthday check) and its run,
     never for a later init, and void once this seat declined the card.
     `WalletView` gains `founding` (the wizard's purse stage follows)
     and `can_watch` (the view key is here). Three more tags:
     `wstat` (`held` true/false and its init, another init's is dropped;
     own status every 30 s and on change;
     the own row is `Held` with the purse's record, else `WatchOnly`),
     `wvask` (every 15 s while no view key opens the address) and
     `wvresp` (any holder, at most once per asker in 10 s). Statuses
     and a handed-over view key are kept in `transport.state`
     (`wallet_status`, `wallet_view`, sealed like `vault_seed`; their own
     writer message, so a cursor save never resets them), not in
     the scan file: a damaged scan file costs a rescan, never the
     view key. A purse adopted without its block hook (a recovery's
     chain) settles on the next beat. §10.29–31, 38b, 40–41 run end to
     end (`tests/wallet_stage.rs`); the status, ask/answer and arming
     rules also as unit tests (`wallet_run_tests.rs`).
8. [x] §7.9 Scanner; manual regtest run (2026-10-10). The scanner
   lives in `molt-engine/src/wallet_scan.rs`: one task while a purse, its
   view key and a dialable daemon exist (fingerprinted; a change restarts
   it), aborted in `reset_workspace_state`. It owns the `ScanState`,
   fetches at most 100 blocks per `get_blocks.bin`
   (`monero_rpc::scannable_blocks`) and reports each round through
   `NetWalletScan`, which gained `connected` and the state's bytes; the
   actor persists changed bytes and fills the view. Polls every 30 s,
   retries from 10 s doubling to 10 min, a fork pause hourly. A decode
   error (`DaemonError::Fault`) shows as `scan_paused = "daemon fault"`
   with `connected` true; only `update needed` emits `WalletScanPaused`.
   History is one row per transaction, newest first; confirmations are
   `daemon_height + 1 - height` (the daemon's top block counts, as in
   Monero's wallet), as the balance rule counts them, capped at the
   scanned chain (fix round: a claimed tip is cheap); only a round that
   read blocks or caught up reports its tip. A time lock opens
   on the newest scanned block's time. The scan layout is
   `molt-wallet-scan-v3`: the purse's address (another purse's file, or
   a purse swapped in session, rescans), each remembered block's time,
   each output's block time (older files read as damage: one rescan).
   Deviation: the task does not run under `net_scope`; its own
   generation drops a stopped task's reports. `molt-treasury`'s
   `test-blocks` feature builds the real blocks the stub daemon serves
   (a payment is a miner output without its lock).
   Regtest: monerod (fakechain) takes mainnet addresses.
   §10 tests: `tests/wallet_scan.rs`, `tests/wallet_regtest.rs`
   (`#[ignore]`, `MOLT_TEST_MONEROD`).
9. [x] §11 UI (2026-10-10). The `Purse` global (`theme.slint`) carries
   one rendered state for the pane, the wizard's last step and the panel;
   `molt-ui/src/wallet.rs` fills it from `WalletView` (amounts, the U2
   line, the QR as `monero:<address>` with a 4-module quiet zone) and
   `actions/wallet.rs` drives the four seat tools and the node
   (`patch_settings`; a non-onion node gets the relay-style
   acknowledgement once, then non-onion dialing goes on like a confirmed
   relay's). Details (U6) are the `status` view, open rather than
   collapsed, above the purse's votes; `settings` is the node field. The
   gather reads the Wallet snapshot even without a nav row, so the
   organization dialog knows the phase. The panel opens on a new run or
   a new consent question; closed, the bar stays in Balance; a workspace
   switch starts that state afresh. Agree/Decline need no node. The
   organization dialog's purse box shows the consent lines and asks for
   the node first; a `wallet_init` card carries the consent lines too.
   The loss dialog opens for the open republic only (its set-aside acts
   there): on its backup hold, once per damage, and on its refused manual
   export; a closed republic's hold is a toast naming it. English refusal
   toasts stay the MCP text but for the daemon and op words (U1).
   Tests: `tests/wallet.rs`, `tests/gui/wallet.rs` (live-preview).
10. [x] clippy per crate = 0; suites green; review over the whole diff;
    master (2026-10-10). The three-instance manual walk (§16) is open,
    in `docs/reviews/known_debt.md`.

## 15. Known traps

- Line numbers drift — grep symbols.
- Do not „tidy up“ the `schnorr-signatures` pin (multiexp split).
- **No new rule in the chain verification**, checkpoint layouts
  unchanged (§7.7).
- The projection reads **only** payload + `CheckpointState`: no
  signature count, no height, no order between surfaces — those
  do not survive a cut.
- `rule_m`/`rule_n` from genesis/anchor, never `State::threshold()`.
- Never derive the marker from the feature value; `set_features` does not
  add `wallet`.
- `wallet_created` needs its own ID; close sibling cards.
- Decline and birthday check in **every** signing path.
- Auto-start only in the hook `after_block_applied`, once per init, never at
  restore; auto-init only at create/join, never recovery; afterwards only
  `WalletRetry`.
- Never replace keys records before the purse is final (I16); reopen with a record never
  aborts.
- `require_feature` also sits in `cmd_approve` — exempt `wallet_init`.
- Do not touch the founding ritual; the stage begins after the seal.
- `relay_kind` knows only `ws`/`wss` — daemon via `daemon_kind`.
- Kind-445 events are not chunked; control frames are small.
- Control frames are not provable to third parties — no public blame.
- `WeakSender` does not end workspace tasks; `net_scope`.
- `MemberSeen` is a no-op.
- Not `monero-simple-request-rpc`, not `GuaranteedScanner`, not the
  `multisig` feature in stage 1.
- The read key does not see the purse.

## 16. Definition of Done (stage 1)

- Tests from §10 green; regtest run once manually, log in the commit.
- clippy per crate = 0; `dev-ui.sh build` clean.
- Manually over three instances: founding with the purse stage as the last
  wizard step, same address, warning visible, mining visible,
  close/reopen, export/import keeps the part, phrase recovery shows
  watch-only; second republic without a purse → enabling later with panel.
- Review over the whole diff, findings fixed, green on master.
- Doc status updated, `check-doc-refs.py` clean.

## 17. Changes against revision 2

- W8: n identity attestations in `wallet_created`; the projection checks from
  the payload; seal at m instead of n, `try_commit` unchanged.
- W4 new: `wallet_init` is the only door; no `wallet_rev` marker;
  `set_features` does not add `wallet`.
- Init (once, chain) and run (ephemeral, run nonce, retry without a block)
  separated; starter/proposer with position fallback.
- W9/W10: purse on by default; purse stage as the automatic last
  wizard step; panel when enabling later.
- W11: mainnet allowed, warning next to the address.
- Usability rules U1–U6 after the official GUI (Simple Mode); QR code;
  daemon suggestion via `wdhint`.
- Review round rev 3: keys record per run (never replaced), daemon first +
  abstention, birthday margin, feature gate also at approve, hook
  `after_block_applied`, auto-init only create/join, run races,
  „Enter republic“ always open, purse checks the init's birthday/network,
  mainnet as default.
- No public blame.
- Daemon: `daemon_kind` from the relay host rule, non-onion gate, no
  default daemon.
- Anchors corrected: `vault_founding_table` instead of a new helper;
  `own_approvals` is a field; `checked_kind` a closure; floor pinned by
  test; `s3::http` are free functions; `rule_m` from genesis.
- Upstream corrected: cause of the pin, pedpop deleted on `next`,
  `SalLegacyAlgorithm`, the scanner belongs to monero-wallet.
