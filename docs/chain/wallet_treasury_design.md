# The Treasury (Wallet Surface)

How a republic gets — and governs — a shared Monero purse. Like
`founding_ritual.md`, this document describes the design **abstractly**: the
actors, the keys, the messages, and the guarantees that hold when each phase
is over. The concrete crates and the build order live in
`docs/chain/multi-sig-wallet-plan.md`.

Status: **REV 2, 2026-10-06** — decisions W1–W7 (§0) ratified by the user;
the protocol revised after a re-check against master `9573c3a`, a compiled
dependency spike, the state of Monero upstream, and an independent review.
Rev 1 (ratified 2026-08-16) is superseded; §13 lists what changed. Nothing
is implemented. **Scope is Stage 1 only:** found the purse, receive, watch.
Spending (Stage 2) is gated on upstream (§8).

---

## 0. Decisions (user, 2026-10-06)

- **W1 Bounds.** The treasury can be enabled only when `2 ≤ m ≤ n − 1`.
  At `m = n` a single lost share would freeze the purse; at `m = 1` the
  DKG would hand every seat the full key.
- **W2 Stage 2 waits.** No spending on CLSAG/FROSTLASS. Spending is built
  once SA+L threshold signing is released in monero-wallet (§8).
- **W3 Monero only.** No Ethereum treasury.
- **W4 A fresh vote.** A `wallet` feature enabled before this design (the
  old wizard default, or MCP) does NOT turn the real treasury on; a new,
  marked `set_features` vote does (§3.1).
- **W5 A share is never silently dropped.** A damaged key file fails the
  export loudly; on import, the restore proceeds without it and the seat is
  watch-only, loudly (§6).
- **W6 One decline kills the init.** Every seat must consent (§3.2).
- **W7 One seat order.** DKG participant numbers are the 1-based positions
  in the founding table — the vault's order — through one shared helper.

## 1. Principle: one threshold

A republic is a fixed group of *n* members with an *m*-of-*n* rule, sealed at
founding and never changed. The treasury inherits that constitution:

- **Every member holds a key share.** The spend key never exists in one
  place; it is born distributed through a DKG among all *n* members.
- **The republic will spend by the same rule it governs by** — `rule_m`, no
  second threshold. (Stage 2.)
- **The chain is the treasury's constitution.** The purse's creation is a
  threshold-signed block.

No signer-set configuration, no key ceremony beyond the one DKG, no
resharing (none exists in the stack; membership is fixed for life).

## 2. Actors and their keys

- **The roster identity key** (Ed25519, per member). Authenticates who
  speaks. No cryptographic role in the wallet.
- **The group spend key** (Ed25519 point, prime order). Minted by the DKG;
  **nobody holds the private scalar**. Each member holds a threshold share
  (`ThresholdKeys<Ed25519>`); *m* shares sign, *m − 1* reveal nothing.
- **The shared private view key** (one scalar, held by every member). **Not
  produced by the DKG** — no library in the stack derives one (verified
  2026-10-06). It comes from a contribution round inside the ritual (§3.3).
  Every member sees the treasury's balance and history; outsiders see
  nothing. This includes agent seats: whoever operates an agent sees the
  treasury. The enable dialog says so.

The group key is never derived from identity keys, phrases or workspace
ids. The view key is never derived from the group key (that would make it
public: anyone with the address could scan the purse — why Serai's public
view key is not an option here).

**No stored outgoing view key.** monero-wallet warns against reusing one
across incompatible transactions, and a re-proposed spend would be exactly
that. Stage 2 draws a fresh one per transaction (§8).

**Participant numbering (W7):** a seat's DKG participant index is its
1-based position in the **genesis founding table** (from the verified
genesis or checkpoint anchor, as the vault reads it). One helper serves
vault and wallet. The chain's "m lowest-named signers" rule stays a sealing
tie-break, not a key ordering.

## 3. Founding the purse ("Kasse einrichten")

```
initiator          every member                          chain
   │ WalletInit        │                                   │
   ├─ propose {wallet_init, birthday} ─────────────────────▶│
   │                   │ approve × n (one decline kills)   │
   │                   │                     sealed at n ◀──│ (init block)
   │          ┌─ readiness (wallet build + daemon) ─────────│
   │          ├─ round 1: commitments + PoP + view contribution
   │          ├─ round 2: encrypted shares + transcript hash│
   │          └─ local completion: keys, view key, address  │
   │          persist keys file, THEN co-sign               │
   ├─ propose {wallet_created, transcript, address …} ─────▶│ sealed at n
   │                   │          scanners start            │
```

### 3.1 Enablement

The real treasury is on only if **all** hold:

- an applied `set_features` entry names `wallet` **and carries the additive
  marker `"wallet_rev": 1`** (W4). Founding feature sets and older entries
  never carry it. The "already enabled" refusal admits `wallet` again when
  no marked enablement exists. (`set_features` entries accumulate across a
  cut, so the marker survives compaction; propose canonicalization rewrites
  only `value` and keeps the marker — both verified.)
- the founding rule satisfies `2 ≤ m ≤ n − 1` (W1);
- the republic is chain-governed.

These are judged in the **projection** from the verified chain, never by a
new rule in chain verification (§3.5). The wizard keeps `wallet` locked;
the vote is the only door. The vote dialog states, one line each: every
member sees the balance; a share lost without a backup is gone; **the purse
can receive but cannot spend until Stage 2 ships**.

### 3.2 Intent and consent — all n

Any member issues `WalletInit`. The engine refuses unless §3.1 holds, no
purse exists, and no init is in flight. It probes its daemon for the height
and proposes `{op: wallet_init, birthday_height}`.

- **Birthday check.** Every approver compares the birthday to its own
  daemon: it must not lie above its own height, nor more than a fixed window
  below it. A lying or forked daemon could otherwise set a future birthday
  and make every scanner miss deposits silently.
- **All n.** The init seals only at **n** signatures (beside the cut rule in
  sealing). Holding a share of the republic's money is an individual
  commitment; PedPoP needs all n anyway.
- **One decline kills** (W6), in every signing path, not only in `approve`.
- **The ritual starts** on a seat only when the init block is applied
  **live** (never on replay or catch-up), carries **n** distinct valid
  signatures, and this seat's own consent still stands. An init sealed at
  m by an older build is inert.

### 3.3 The rounds

The rounds travel as **control frames** over the MLS group, resent on the
presence tick until the purse commits or the ritual aborts. They never
enter the workspace log, so "no trace" holds and older builds are not fed
unknown events.

0. **Readiness.** Every seat announces a wallet-capable build and a
   reachable daemon. Missing announcements by the deadline abort without
   blame.
1. **Round 1** (broadcast): PedPoP commitments + proof of possession, and a
   fresh 32-byte **view contribution** `c_i`. Commitments with the identity
   point are rejected before they reach the library.
2. **Round 2**: per-recipient secret shares (encrypted by the DKG protocol),
   plus the sender's **transcript hash** `T = H("molt-wallet-transcript-v1"
   ‖ every round-1 message in participant order)`. A seat that sees a
   different `T` from anyone aborts.
3. **Local completion.** Each node derives its `ThresholdKeys` (identical
   group key everywhere), the view key, and the **standard** main address
   from (group key, view key).

**Bindings, all length-prefixed and entry-counted:**

- PedPoP context: `H("molt-wallet-dkg-v1" ‖ republic_id ‖ init_id ‖ t ‖ n)`
  (the library asks for a context unique per multisig).
- `view = H_s("molt-wallet-view-v1" ‖ republic_id ‖ init_id ‖ (i ‖ c_i) for
  i in participant order)`.
- Integer and id encodings are fixed and pinned by byte-pin tests.

**Why the transcript hash.** PedPoP does not detect a sender who hands
different commitments to different parties ("responsibility lies with the
caller"), and requires confirming successful completion with all
participants. MLS authenticates senders but does not guarantee every member
saw the same message (a sender can publish two different events to disjoint
relays). `T` in round 2 and in the terminal block closes both gaps; a
second, differing round-1 frame from one sender is equivocation and aborts.

**Bias.** The last contributor can choose `c_i` after seeing the others.
That is harmless: the hash makes no chosen view key reachable, and the view
key's secrecy rests on the MLS group's confidentiality, not on randomness
against members — every member holds it anyway.

Round state cannot be persisted (the library's machines have no
serialization). Before round 2 is sent, a crash, timeout, decline or blame
ends the ritual with no trace. **After** a seat sent round 2, it keeps its
state until the purse commits or the init is superseded; a local timeout
alone does not discard it.

### 3.4 Persist, then attest

Each node writes its **keys file** (share, view key, birthday, init id)
through the blocking writer **before** it signs anything, then proposes or
co-signs the terminal change:

```json
{"op": "wallet_created", "init": <init id>, "transcript": "<hex T>",
 "address": "<standard main address>",
 "threshold": <rule_m>, "participants": <n>, "birthday_height": <h>}
```

- **It seals at n**, like the init. A purse therefore exists only if every
  seat computed the same transcript and address and holds its share; W1's
  one-loss margin is real from the first block.
- **One proposer.** The seat at founding position 1 proposes; if no
  proposal appears within the deadline, position 2, and so on. Every other
  seat auto-co-signs only an exact match with its own computation.
- **First wins, siblings die.** The projection takes the first committed
  `wallet_created` per init and ignores later ones; on that commit every
  sibling card for the same init is settled (as stale vault cards are), so
  no re-sign revives it.
- The projection also checks that the init exists and that `threshold` and
  `participants` match the genesis.
- A seat whose computation differs never signs; the purse does not form;
  the init dies and a fresh `WalletInit` is allowed.
- **Restart between persist and seal:** on reopen the engine finds its keys
  file and an uncommitted purse, and re-co-signs a matching pending
  `wallet_created`. A seat that closed before round 2 aborts on reopen.

### 3.5 What chain verification does NOT do

The wallet adds **no rejection rule to chain verification**. Older builds
accept any Wallet op, legacy republics already carry mock Wallet blocks
(the old `transfer` op), and verification is all-or-nothing: a new rule
would reject existing chains or fork a republic the moment an older seat
seals something. Instead:

- the closed op set (`wallet_init`, `wallet_created`) is enforced at
  propose, approve, wire ingest and in every signing path;
- the projection deterministically **ignores** any other Wallet op, anything
  before the marked enablement, an init without n signatures, and every
  `wallet_created` but the first per init.

## 4. What the chain learns — and what it never does

On the chain: address, transcript hash, threshold, participants, birthday,
init id. The address carries the view **public** key, so a seat that later
receives the view key checks `view·G` against it — no separate commitment
is needed. Never on the chain or in any log: the view key, any share or
`ThresholdKeys` material.

## 5. Status, recovery

- **Shareholder status** is a per-seat control frame (share held /
  watch-only), persisted per member and re-sent on the presence tick.
  Unknown shows as unknown.
- **Phrase-only recovery: watch-only.** No resharing exists. A seat
  recovered without a backup regains identity and vote, asks the group for
  the view key by an ask/answer control frame after rejoin (checked against
  the address), and sees balance and history — but holds no share. The
  purse keeps its power while at least m shares live (W1 guarantees one
  loss is survivable).
- **Emergency exit (recorded, not built).** m share holders together can
  reconstruct the full spend key (`dkg-recovery`) and sweep the purse with
  an ordinary wallet. It deliberately breaks "never assembled" and exists
  only for when Stage 2 is unavailable and funds must move. A future design
  pass decides whether it gets a governed path.

## 6. Persistence and backup

Two files, because one secret must never be lost and the rest is cheap:

- **Keys file** — share, view key, birthday, init id. Written **once**, at
  the end of the ritual, through the blocking writer. Encrypted on the
  `chain.state` pattern (own HKDF sub-key, own AAD segment, atomic
  replace, zeroized in memory).
- **Scan file** — cursor, recent block hashes (reorg detection), seen
  output keys, scanned outputs. Rewritten as scanning advances. Damage
  costs a rescan from the birthday, nothing else.

**Backup (W5):**

- The export authenticates the keys file. If it does not authenticate, the
  export **fails** with one line naming the file — a backup that silently
  lost the share would be worse than none. The S3 ticker backs off on this
  failure instead of retrying every minute; retention prunes only after a
  successful upload, so good copies are kept.
- **Way out of a damaged file:** the seat acknowledges the loss once; the
  file is set aside, the seat is watch-only, and exports resume.
- The import authenticates the keys file. If it does not authenticate, the
  restore proceeds without it and the seat is **watch-only**, loudly.
- The scan file follows the base pattern: shipped if it authenticates,
  skipped and named if not.
- A backup plus its secret is therefore a full share, as it already is a
  full seat. The backup dialog says so.
- Version skew: an older build exports without the keys file (it does not
  know it) and refuses to import a blob that carries it. Release notes say
  so.

## 7. Watching the purse

- **Standard scanning.** A standard main address and the library's ordinary
  `Scanner` with a `ViewPair` — not the guaranteed scanner, whose featured
  addresses ordinary wallets do not pay correctly. The scanner keeps the
  **seen output keys** and ignores any repeat (documented as mandatory
  against the burning bug). It needs only the spend public key and the
  view key.
- **Main address only.** Subaddresses with multisig are unverified upstream.
- **Confirmations: 20.** Shown as pending below (the network saw an
  18-block reorg in September 2025). A reorg is detected against the stored
  recent block hashes; then the scan rewinds or rescans from the birthday.
- **The daemon** is the member's choice. It is reached through
  monero-daemon-rpc with a transport over the project's HTTP client and
  dialer; which hosts count as local follows the existing relay
  classification (loopback, private, link-local), not a separate list.
  Daemon login and response-size limits are handled there.
- A lying daemon can hide funds from that member or lag; it cannot fake
  ownership. n independent scanners are the cross-check.
- **Network** is configurable (mainnet, stagenet, testnet) for tests; the
  republic's purse records which.

## 8. Stage 2 — spending (gated, not designed here)

Built only when all hold (W2):

1. monero-wallet ships **SA+L threshold signing** over
   `ThresholdKeys<Ed25519>` (today an unreleased branch, not wired into the
   wallet crate);
2. a security proof or audit for threshold SA+L exists;
3. the FCMP++ fork heights are fixed.

Fixed now, whatever the algorithm:

- the proposal carries the exact transaction; every engine decodes
  destination and amount from those bytes (sign-what-you-see);
- ratification is the ordinary m-of-n;
- signers are chosen by an explicit readiness step, never by a local guess
  of who is online; watch-only seats are excluded;
- a fresh outgoing view key per transaction;
- a stale transaction dies and is re-proposed, never patched;
- spend blocks carry a hash and the txid, not the transaction bytes (the
  checkpoint must stay under the gift-wrap cap);
- spent detection: key images need an m-party computation per output, and
  watch-only seats can never do it alone. The design pass decides how the
  balance reconciles against spends outside the app.

## 9. The Monero fork (FCMP++ + Carrot)

- Same hard fork: v17 transition, v18 final. No mainnet date (2026-10-06).
- **Keys and address survive.** Carrot keeps existing addresses; the
  legacy-hierarchy structure (DKG spend key + shared view key) stays valid.
- **The scanner does not.** monero-oxide 0.1.0's scanner refuses blocks
  above hard-fork version 16 (`UnsupportedProtocol`), and its decoder fails
  on unknown output types. The scanner treats `UnsupportedProtocol` as
  **"scanning paused: update needed"** and a decode error as a daemon fault
  (otherwise a lying daemon could fake the pause). Funds are safe meanwhile;
  only the display stops. The scanner sits behind the treasury's own
  interface, so the Carrot upgrade swaps an implementation.
- **Spending after v18** needs SA+L (§8). CLSAG transactions become
  invalid; this is why W2 waits.

## 10. Load-bearing invariants — do not weaken

- **I1 One threshold.** Spend authority is `rule_m`.
- **I2 Sign-what-you-see.** A seat co-signs `wallet_created` only over values
  its own DKG computed, including the transcript.
- **I3 Secrets never touch the chain or the log.** View key, shares.
- **I4 Persisted before attested.** The keys file is on disk before the
  seat's co-signature exists.
- **I5 One-shot init.** Never resumed; failure means a new id.
- **I6 All n.** The init and the purse both seal at n; one decline kills.
- **I7 The group key is DKG-born.** Never derived, never assembled — the
  emergency exit (§5) is the one recorded exception, not built.
- **I8 A share exists exactly once** — in its member's keys file and that
  member's own backup. No escrow; the vault must not hold wallet shares.
- **I9 Library discipline.** Unique DKG context; identity points rejected;
  transcript confirmed with all participants before completion.
- **I10 Main address only.**
- **I11 Deterministic sealed values.** Participant order, birthday, block
  fields come from ratified intent or DKG output.
- **I12 Never lose a share silently.**
- **I13 No new verification rule.** The wallet's rules live in propose,
  approve, ingest, signing and projection — never in chain verification.

## 11. Failure and abort

| failure | behaviour |
|---|---|
| not enabled, legacy enablement, bounds | `WalletInit` refused, naming the reason |
| birthday outside the approver's window | that seat declines (one decline kills) |
| one member declines | init dead, no rounds |
| a seat not ready | abort before round 1, no blame |
| timeout before round 2 | abort, no blame, no trace |
| equivocation (two round-1 messages, differing transcripts) | abort; the sender is named only with both signed frames as proof |
| invalid share or identity point | abort; a blame claim is broadcast only with the library's proof, else no name |
| crash before round 2 | no trace; re-mint |
| crash after persist, before seal | reopen re-co-signs the matching purse |
| a seat computed a different result | it never signs; purse does not form; re-init allowed |
| daemon unreachable | status shows it; scanning resumes later |
| fork reached, scanner outdated | scanning paused, loud, funds safe |
| keys file damaged at export | export fails; acknowledge to set it aside, then watch-only |
| keys file damaged at import | restore without it, watch-only |
| scan file damaged | rescan from the birthday |

## 12. Dependencies and threat notes

- **Stage 1 stack** (MIT, pure Rust, verified by a compiled spike
  2026-10-06, no `ring`, no C): `dkg` 0.6.1, `dkg-pedpop` 0.6.0,
  `dalek-ff-group` 0.5 + `ciphersuite` 0.4 (the Ed25519 suite),
  `monero-wallet` 0.2.0 **without** the `multisig` feature,
  `monero-daemon-rpc` 0.2.0. Stage 1 needs neither `modular-frost` nor
  monero-wallet's `multisig` (outside SemVer); Stage 2 adds what SA+L
  requires.
- `monero-simple-request-rpc` is out: it pulls `ring` and cannot use the
  project's dialer.
- **`dkg-pedpop` is orphaned upstream** (removed from Serai, no new home)
  and needs a pin to build (`schnorr-signatures = "=0.5.2"`). Its last
  audit (2023) covered older code. The project reviews the pinned version,
  rejects identity points itself, and watches `dkg-evrf` as a successor.
- **monero-oxide's bug bounty is paused.** Pin minors, upgrade deliberately.
- **What an attacker gets** from one member's device: one share (< m: no
  spend), the view key (privacy loss toward that attacker), a vote. From a
  daemon: traffic analysis, lag, at most a refused birthday. From m
  devices: everything — unchanged from governance.

## 13. What changed against rev 1

- Scope narrowed to Stage 1; Stage 2 gated on SA+L.
- The view key comes from a contribution round, not the DKG; no stored
  outgoing view key.
- Init AND terminal block seal at n; transcript hash against equivocation;
  unique DKG context; round state kept after round 2.
- One proposer for the terminal block, first wins, siblings settled; own
  proposal id.
- No new chain-verification rule; closed op set enforced elsewhere and in
  the projection.
- Rounds as control frames, not logged events; readiness round first.
- Birthday checked by every approver.
- Enablement `2 ≤ m ≤ n − 1` and a marked vote.
- Two files: write-once keys, rebuildable scan; damaged keys fail the
  export, with an acknowledge path; restore marks watch-only.
- Standard scanner with seen keys and block hashes; 20 confirmations;
  `UnsupportedProtocol` = fork pause.
- Recovery gets the view key by ask/answer frame, checked against the
  address; status by control frame.
- RPC: monero-daemon-rpc over the project's client and dialer; local hosts
  by the relay classification.
- Stage 1 stack without `modular-frost` and without the `multisig` feature.
- Participant order = genesis founding table, shared with the vault.
- FCMP++/Carrot consequences (§9) and the emergency exit (§5) are new.
