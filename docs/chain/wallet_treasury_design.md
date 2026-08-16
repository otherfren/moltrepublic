# The Treasury (Wallet Surface)

How a republic gets — and governs — a shared Monero purse. Like
`founding_ritual.md`, this document describes the design **abstractly**: the
actors, the keys, the messages, and the guarantees that hold when each phase
is over. The concrete crates and the build order live in
`docs/chain/multi-sig-wallet-plan.md`; the code is pointed to at the end.

Status: RATIFIED 2026-08-16 (drafted 2026-07-18; revised 2026-08-16 against
the current code: charter-feature gate, Nostr transport, at-least-once
delivery). Nothing below is implemented yet — build order in
`multi-sig-wallet-plan.md` §15, next step is the dep-lock spike (§6 there).

---

## 1. Principle: one threshold

A republic is a fixed group of *n* members with an *m*-of-*n* approval rule,
sealed at founding and never changed. The treasury inherits exactly that
constitution:

- **Every member holds a key share.** The wallet's spend key never exists in
  one place; it is *born distributed* through a DKG among all *n* members.
- **The republic spends by the same rule it governs by.** A spend requires
  *m* approvals — the chain's `rule_m`, not a second, wallet-specific
  threshold. There is ONE threshold concept in the whole system.
- **The chain is the treasury's constitution too.** The wallet's creation is
  a threshold-signed chain block; every spend (Etappe 2) is proposed,
  ratified, and recorded through the same persistent-change chain that
  governs everything else (`persistent_chain.md`).

What falls out of this: no signer-set configuration surface, no key
ceremonies beyond the one DKG, no resharing protocol (none exists in the
chosen stack, and none is needed — membership is fixed for life, a deliberate
match).

One precondition sits in front of everything below: the Wallet surface is an
**optional charter feature** (`docs_archive/ritual/charter_features.md`) —
enabled at founding or by a later `set_features` threshold vote, never
disabled again. A republic that never enabled it has no treasury surface and
`WalletInit` is refused; the ritual of §3 presumes the feature is on.

## 2. Actors and their keys

Three kinds of key material exist, and they must never be conflated:

- **The roster identity key** (Ed25519, per member, from the founding
  ritual). It authenticates *who speaks*: DKG round messages, approvals, and
  chain signatures are attributable to a member through it (via the MLS mesh
  and the chain's `identity_pk` anchors). It plays **no cryptographic role**
  in the wallet itself.
- **The group spend key** (Ed25519 point, one per republic). Minted by the
  DKG; **no one ever holds the private scalar** — each member holds a
  *threshold share* (`ThresholdKeys`). *m* shares cooperate to sign a CLSAG
  (FROSTLASS); *m−1* shares reveal nothing.
- **The shared private view key** (one scalar, held by *every* member).
  Deliberately *not* threshold: every member can independently scan the
  chain and see the treasury's balance and history. Radical transparency
  toward members is a feature — the treasury has no private corners *inside*
  the republic, while outsiders see nothing.

The derivation lesson from the founding ritual applies verbatim: the group
key is **never derived from** a member's identity key or phrase. It comes
out of the DKG and only out of the DKG. (A re-derivation shortcut would give
the wrong key and, worse, would couple treasury security to identity-key
handling.)

Participant numbering: the DKG and FROST address members as `Participant`
indices 1..=n. The mapping is the roster's member names in ascending byte
order — the *same* ordering the chain already uses to pick the "m
lowest-named signers" deterministically. One ordering, defined once, used
everywhere.

## 3. The founding of the purse ("Kasse einrichten")

A one-shot, all-n ritual layered on the existing governance machinery.
Nothing about it invents a new consensus path: the *intent* is an ordinary
gated proposal, the *consent* is ordinary approvals, and the *result* is an
ordinary `Applied` chain block. Only the middle — two DKG rounds — is new,
and it rides the same encrypted gossip as everything else.

```
initiator            every member                       chain
   │  WalletInit         │                                │
   │  (daemon probe → birthday height)                    │
   ├── Proposed {op: wallet_init, birthday_height} ──────▶│ (pending)
   │                     │  Approve  (× n — ALL seats)    │
   │                     │  a single Decline aborts       │
   │            ┌── round 1: commitments + PoP (broadcast)│
   │            ├── round 2: encrypted shares (broadcast) │
   │            └── local completion: ThresholdKeys,      │
   │                 address, view scalar — all derived   │
   │                 INDEPENDENTLY on every node          │
   │            persist wallet.state, THEN co-sign        │
   ├── Applied {op: wallet_created, address,              │
   │            view_key_commitment, m, n, birthday} ────▶│ sealed at m
   │                     │        scanners start          │
```

Phase by phase:

1. **Intent.** Any member issues `WalletInit`. The engine refuses if the
   wallet charter feature is not enabled, if a wallet already exists, or if
   an init is in flight (one-shot). It probes the
   configured daemon for the current height and mints a normal pending
   proposal on the Wallet surface carrying `birthday_height` (probe height
   minus a safety margin). Binding the birthday into the ratified intent
   makes every member's scan start deterministic.
2. **Consent — all n, not m.** The proposal is approved through the
   ordinary approve/decline verbs, but the DKG starts only when **all n**
   members have signed the intent. Holding a share of the republic's money
   key is an individual commitment; PedPoP needs all n participants anyway;
   and sign-what-you-see demands each member consent to *this* wallet
   (this birthday, this rule) explicitly. A single decline aborts the
   ritual. The n collected signatures ratify the *intent*; they are not the
   terminal block's signatures.
3. **Round 1.** Each engine constructs its PedPoP machine and broadcasts
   its commitments + proof of possession over the mesh, addressed by
   `init_id`.
4. **Round 2.** Once all n−1 peer commitments arrived, each engine
   broadcasts its per-recipient secret shares. These are encrypted by the
   DKG protocol itself (per-message ephemeral keys); the MLS group channel
   supplies the sender authentication PedPoP requires. Broadcasting the
   ciphertexts to the whole group is therefore safe — a member can only
   decrypt the share addressed to it.
5. **Local completion.** With all shares in, every engine independently
   derives: its `ThresholdKeys`, the group's main address, the shared view
   scalar, and `view_key_commitment = SHA-256("molt-wallet-view-v1\0" ‖
   scalar)`. Nobody is told the result — everybody *computes* it
   (sign-what-your-own-DKG-computed).
6. **Persist, then attest.** Each engine writes `wallet.state` (its share,
   the scalar, the birthday) **before** signing anything. Then it
   auto-co-signs — checkpoint-style, only over its own computed values —
   the chain change `Applied {op: wallet_created, address,
   view_key_commitment, threshold: rule_m, participants: n,
   birthday_height}` under the intent's proposal id. Every field is
   deterministic DKG output or ratified intent, so all n candidate blocks
   are byte-identical; the existing machinery seals at *m* and broadcasts.
   A node that computed a *different* address simply never co-signs a
   conflicting block — a diverging DKG cannot be papered over.
7. **Alive.** `WalletReady` fires, scanners start (§6). The purse can now
   receive; it cannot yet spend (Etappe 2).

The ritual is **ephemeral until step 6**: timeout, decline, a blamed
misbehaver, a crash — nothing has touched disk, the proposal dies, and a
fresh `WalletInit` starts over with a new id (cancel-and-re-mint, exactly
the founding's `CreatePropose` semantics; a half-run DKG is never resumed).

## 4. What the chain learns — and what it never does

The terminal block is public *within the republic* (and inside any backup a
member exports). It carries:

| field | why it is safe on the chain |
|---|---|
| group main address | it is what payers are given anyway |
| `view_key_commitment` | a hash; lets a recovered member *verify* a scalar served to it without the chain storing the scalar |
| `threshold`, `participants` | mirror of the genesis rule (redundant on purpose: the block is self-describing) |
| `birthday_height` | scan-start determinism; reveals only "the purse existed after height X" |

**Never on the chain, never in any `WorkspaceEvent` payload beyond the
protocol-encrypted round 2:** the private view scalar, any `ThresholdKeys`
material, any DKG share. The split is the one the recovery ritual already
established: *the chain proves authenticity, MLS carries confidentiality.*
A member that needs the view scalar (a phrase-only recovery, §5) receives
it over the MLS recovery channel from any peer and verifies it against the
on-chain commitment before trusting it.

Note on the DKG rounds riding the workspace log: round payloads are either
public-by-design (commitments, proofs of possession) or protocol-encrypted
(round-2 shares). Their presence in the log leaks *that* a wallet was
founded and *by which members* — which the terminal block states anyway. A
log-free side channel remains listed future work for the chain in general;
the treasury does not need it sooner.

Transport notes (post-N4/N5): the gossip rides the production Nostr
transport as MLS-encrypted group traffic — the same path as all wire
events — and delivery is **at-least-once** end-to-end
(`docs_archive/transport/delivery_guarantee.md`). Duplicate round messages
arrive by design (rewind-resend), so round ingestion is idempotent by
construction, not as a defensive nicety.

## 5. Persistence, backup, recovery

- **`wallet.state`** — a new encrypted per-workspace file, the exact
  `chain.state` pattern: sub-key HKDF-derived from the workspace key (tag
  `molt-wallet-state`), own AAD segment, atomic replace, mode 0600,
  zeroized in memory. Contents: the member's `ThresholdKeys`, the shared
  view scalar, the group address, birthday, scan cursor, and the scanned
  output set. Written at DKG completion, then read-modify-write as the
  cursor advances and on clean close.
- **Backup: included.** `wallet.state` travels in the workspace export as
  verbatim ciphertext (portable — its sub-key derives from the workspace
  key), alongside `chain.state`. It must NOT live in `transport.state`,
  which is hard-excluded from backup by design. A member restoring from a
  backup gets its share, the scalar, and its cursor back: fully
  functional.
- **Phrase-only recovery: watch-only, honestly.** The chosen DKG stack has
  no resharing; a share lost with the device is gone. A member recovered
  through the recovery ritual without a backup: regains its identity and
  seat, receives the view scalar over MLS (verified against the
  commitment), sees balance and history, votes on spends — but **cannot
  co-sign transactions**. The republic keeps its spending power as long as
  at least *m* live shares remain; the status view shows share-held /
  watch-only per member, and shows "unknown" rather than guessing.
  This limit is a consequence of "membership fixed for life" and is stated
  to users, not hidden. (If ever needed, the escape hatch is governance:
  found a *new* purse via a fresh `WalletInit`-style ritual and sweep the
  funds — an Etappe-2+ decision, not designed here.)

Crash points, by construction: before step 3.6's persist → no trace, ritual
re-mintable. Between persist and seal → the share is safe on disk; the
block arrives via normal chain catch-up. After seal → steady state.

## 6. Watching the purse

Every member scans independently with the shared view key:

- **Guaranteed view pair.** Scanning uses the stack's guaranteed-output
  scanner, which closes Monero's burning-bug class for received outputs.
  The treasury uses its **main address only**; subaddresses are out of
  scope (their interaction with the multisig path is unverified upstream —
  we do not build on unverified crypto).
- The scanner is an off-actor task per open workspace: it polls the
  member's configured daemon from `max(birthday, cursor)`, and reports
  outputs and daemon health back into the engine as internal commands. The
  engine's projection turns that into balance (confirmed vs. <10-conf
  pending, Monero's unlock window), history, and status for the read
  model. In Etappe 1 nothing can be spent, so every received output is
  unspent by construction.
- **Trust model:** the daemon is the member's own choice (local node,
  onion). A lying daemon can *hide* incoming funds from that member or lag
  its view; it cannot fake ownership (outputs verify against the view
  pair) and it never sees a secret beyond what any daemon sees from any
  wallet. Members comparing balances out-of-band detect a hiding daemon —
  n independent scanners are a feature of the shared-view design.
- Reorg stance (Etappe 1): scanning is idempotent from the cursor;
  confirmations below the unlock window are displayed as pending, and a
  suspected reorg falls back to a rescan from the birthday. No spend
  decisions hang off scan state in Etappe 1, so a stale view is a display
  issue, not a safety issue.

## 7. Spending (Etappe 2 — specified here, built later)

The spend flow composes three existing guarantees — sign-what-you-see,
byte-identical ratification, deterministic signer selection — with the
2-round FROST machine:

1. **Proposal = the exact transaction.** The proposing member builds the
   full `SignableTransaction` (inputs, decoys, fee) against its daemon and
   puts the **serialized bytes** into a gated Wallet proposal, alongside
   the human-readable intent (destination, amount). Every member's engine
   re-parses destination/amount *from those bytes* — what you ratify is
   what will be signed, not a summary the proposer wrote.
2. **Ratification = the ordinary m-of-n.** Approvals collect chain
   signatures over the proposal exactly like any gated change. Sealing the
   `Applied` block fixes *which* transaction bytes the republic authorized.
3. **Signing = the m lowest-named online share-holders** (the chain's
   existing determinism rule) run FROST round 1 (fresh preprocess per
   attempt — caching is forbidden for the transaction machine; one machine
   per input; empty message, the machine derives the real one) and round 2
   (signature shares; an invalid share is attributable → blame event).
4. **Broadcast and record.** `.complete()` yields the transaction; it is
   handed to a daemon; the txid lands in a follow-up block; the spent
   outputs are marked locally.
5. **Staleness fails honestly.** Decoys and fees age; a transaction that
   no longer validates is not patched — the proposal dies and a fresh one
   is built and re-ratified. (Re-signing altered bytes without
   re-ratification would break invariant I2.)

Open Etappe-2 questions, deliberately not answered here: change handling
and output management policy, fee-bump UX, partial-signer retry (a chosen
signer goes offline between rounds), and whether ratification and signing
should overlap in time. These get their own design pass before Etappe 2.

## 8. Load-bearing invariants — do not weaken

- **I1 — One threshold.** Spend authority is `rule_m` of the genesis. No
  second threshold, no per-wallet signer subset, no configuration surface.
- **I2 — Sign-what-you-see, twice.** (a) Ritual: a member only ever
  co-signs the `wallet_created` block over values *its own* DKG computed.
  (b) Spend: members ratify the exact transaction bytes, and only those
  bytes are FROST-signed.
- **I3 — Secrets never touch the chain.** View scalar, shares,
  `ThresholdKeys`: never in a block, never in a plaintext event payload.
  Chain = authenticity (commitments), MLS = confidentiality (delivery).
- **I4 — Ephemeral until persisted, persisted before attested.** No disk
  trace before DKG completion; `wallet.state` is written before the
  member's block co-signature exists.
- **I5 — One-shot init.** A `wallet_init` proposal is never reused or
  resumed; any failure path is cancel-and-re-mint with a fresh id.
- **I6 — All n consent to the DKG.** Not m. A seat that never approved
  never holds a share it didn't agree to hold.
- **I7 — The group key is DKG-born.** Never derived from identity keys,
  phrases, or workspace ids; never reconstructable by fewer than m shares
  (the stack's own guarantee — we add no recovery backdoor).
- **I8 — Independent shares stay independent.** A share exists exactly
  once: in its member's `wallet.state` (and that member's own encrypted
  backup). No share escrow, no share forwarding, no "helpful" copies.
- **I9 — FROST discipline.** Empty message argument; fresh preprocess per
  signing attempt; no preprocess caching for transaction machines; one
  machine per input. (These are the stack's documented safety conditions.)
- **I10 — Main address only** until subaddress-with-multisig is verified
  upstream.
- **I11 — Deterministic everything that is sealed.** Participant mapping,
  birthday, block payload field set: any value entering a signed artifact
  is derived from ratified intent or DKG output, never from local clocks,
  local RNG, or daemon responses at attestation time.

## 9. Failure and abort

| failure | behavior |
|---|---|
| wallet feature not enabled | `WalletInit` refused (feature-disabled); enable it via a `set_features` vote first |
| a member declines the intent | ritual aborts before any round; proposal declined |
| timeout (member offline during rounds) | abort event (no blame), no disk trace, re-mintable |
| invalid share / bad commitment | PedPoP blame machine attributes the sender; abort event names the member — visible in the republic, same social contract as a failed founding |
| crash before completion | nothing persisted; ritual is gone (in-memory), re-mint |
| crash after persist, before seal | share safe; block arrives via chain catch-up |
| daemon unreachable | status view shows disconnected; scanning resumes on reconnect; init refuses to start without a height probe |
| `wallet.state` damaged/missing | loud warning, member treated as watch-only (§5) — never silently re-derived, because it *cannot* be re-derived (I7) |
| diverging DKG result on one node | that node never co-signs the majority block and raises loudly; the republic seals at m without it — the divergent node is effectively watch-only until re-founding; this is detectable, not silent |

## 10. Dependencies and threat notes

- **Stack:** `dkg` + `dkg-pedpop` + `modular-frost` and
  `monero-wallet` 0.2.0 with the `multisig` feature +
  `monero-simple-request-rpc` — since monero-oxide's coordinated 0.1.0
  ecosystem release (2026-07-31) **all published on crates.io**, MIT; no
  git pinning needed anymore. FROSTLASS carries a Cypher Stack security
  proof (IACR ePrint 2026/589) and a completed implementation audit (May
  2025, in monero-oxide's `audits/`). Pure Rust throughout — the posture
  holds.
- **Pinning:** versions are pinned in the workspace manifest with the
  minor held exactly — upstream states the multisig functionality is
  "not covered by SemVer, except along minor versions". Upgrades are a
  deliberate act: re-run the dep-matrix check (single `modular-frost`/
  `dkg`/dalek versions in `cargo tree`), re-read the upstream diff of the
  `multisig`-feature code, re-run the full wallet test suite. Never float
  a minor.
- **Horizon — now near:** CLSAG yields to FCMP++, and the fork has moved
  from "eventually" to "in active integration" (2026-08: mainnet not yet
  activated, no date fixed, second beta stressnet running since May;
  the implementation lives in monero-oxide's `fcmp++` branch). FCMP++
  deliberately migrates no wallets/addresses/outputs, so Etappe 1 —
  the DKG-born group key, the address, the shares — survives the fork;
  scanning needs a dependency upgrade at fork time to parse the new
  transaction format. Etappe 2's signing algorithm is the fork-sensitive
  part: if the fork is activated or scheduled by then, build the spend
  flow on the FCMP++ GSP multisig (2-round, FROST-inspired, same author)
  instead of FROSTLASS/CLSAG — same threshold shares, different signing
  protocol, its own audit situation to check in that design pass.
- **What an attacker gets:** compromising one member's device yields that
  member's share (< m: no spend), the view scalar (privacy loss toward
  that attacker — same blast radius as today's chat history on a
  compromised device), and a voting seat — the same trust the republic
  already places in a member's device. Compromising the daemon yields
  traffic analysis and view-lag, no key material. Compromising m devices
  is game over by definition of m-of-n — unchanged from governance.

## 11. Implementation map

Everything concrete — crates, modules, exact contract additions, storage
format, TDD schedule, UI panes, build order, known traps — lives in
`docs/chain/multi-sig-wallet-plan.md`. Anchors: `crates/molt-treasury`
(new: roster/dkg/keys/rpc/scan), `crates/molt-core/src/wallet.rs` (new:
read-model + op constants), `crates/molt-engine` (`wallet_ritual` next to
`net_ritual`, round arms in `cmd_net_delivered`, checkpoint-style
auto-co-sign), `crates/molt-storage` (`wallet.state`, backup include),
`crates/molt-mcp` (`wallet_init` tool + INTERNAL entries),
`crates/molt-ui-window/ui/surfaces.slint` (`WalletPane`, real data).
