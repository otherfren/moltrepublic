# The Treasury (Wallet Surface)

How a republic gets — and governs — a shared Monero purse. Like
`founding_ritual.md`, this document describes the design **abstractly**: the
actors, the keys, the messages, and the guarantees that hold when each phase
is over. The concrete crates and the build order live in
`docs/chain/multi-sig-wallet-plan.md`.

Status: **REV 3, 2026-10-06** — decisions W1–W11 (§0) by the user. Rev 3
answers the review of rev 2: the purse record proves its own all-n consent
(§3.5), the init vote is the only door (§3.1), every republic gets a purse
by default (§3.2), and the purse runs on mainnet behind a loud warning
(§7). Rev 1 (2026-08-16) and rev 2 (2026-10-06) are superseded; §13 lists
what changed. Built so far: the dependency lock, `molt-treasury`'s pure
core (DKG wrapper, view key, address, attestations, keys record, scan
core), `daemon_kind`, the keys and scan files with export and
import, and the contract, MCP tools and `[wallet]` config (plan §14
steps 2-5, 2026-10-09); the init vote with its daemon transport (step 6,
2026-10-10); the run, the attestations, the purse record and the
restart (step 7a, 2026-10-10); the founding's purse stage, the status
frames and the view key for a seat without a key part (step 7b,
2026-10-10); the scanner (step 8, 2026-10-10). **Scope is Stage 1 only:** found the
purse, receive, watch. Spending (Stage 2) is gated on upstream (§8).

---

## 0. Decisions (user, 2026-10-06)

- **W1 Bounds.** A purse exists only when `2 ≤ m ≤ n − 1`. At `m = n` a
  single lost share would freeze it; at `m = 1` the DKG would hand every
  seat the full key.
- **W2 Stage 2 waits.** No spending on CLSAG/FROSTLASS. Spending is built
  once SA+L threshold signing is released in monero-wallet (§8).
- **W3 Monero only.** No Ethereum treasury.
- **W4 Only an explicit wallet act creates a purse.** A `wallet` feature
  enabled before this design (the old wizard default, or MCP) creates
  nothing. The door is a `wallet_init` vote on the Wallet surface; a
  `set_features` vote can no longer add `wallet`, and there is no marker
  field (a marker inferred from the feature value would let any later
  feature vote switch the purse on — review of rev 2).
- **W5 A share is never silently dropped.** A damaged keys file fails the
  export loudly; on import, the restore proceeds without it and the seat is
  watch-only, loudly (§6).
- **W6 One decline kills the run.** Every seat must consent (§3.3).
- **W7 One seat order.** DKG participant numbers are the 1-based positions
  in the genesis founding table — the vault's order, one shared helper.
- **W8 The purse record proves itself.** `wallet_created` carries all n
  seats' identity signatures over the result; the projection checks them
  from the payload alone, so the check survives every checkpoint cut (§3.5).
- **W9 A purse by default.** The founding wizard's wallet option is on by
  default (locked off, with the reason, when W1 fails). The purse stage
  with its progress bar is the wizard's automatic last step, for every
  participant (§3.2).
- **W10 Enabling later shows the same stage.** Every seat sees the progress
  bar; all n must be online at the same time.
- **W11 Mainnet in Stage 1.** Allowed. Next to the deposit address, in large
  letters: spending does not work yet, money sent here is gone (§7).

## 1. Principle: one threshold

A republic is a fixed group of *n* members with an *m*-of-*n* rule, sealed at
founding and never changed. The treasury inherits that constitution:

- **Every member holds a key share.** The spend key never exists in one
  place; it is born distributed through a DKG among all *n* members.
- **The republic will spend by the same rule it governs by** — `rule_m`, no
  second threshold. (Stage 2.)
- **The chain is the treasury's constitution.** The purse's creation is a
  threshold-signed block that carries every member's attestation.

No signer-set configuration, no key ceremony beyond the one DKG, no
resharing (none exists in the stack; membership is fixed for life).

## 2. Actors and their keys

- **The roster identity key** (Ed25519, per member). Authenticates who
  speaks, and **signs the seat's attestation** of the DKG result (§3.5).
  A recovery never moves it (a `Restored` block may not), so the
  attestation key of a founding position is the same for the republic's
  whole life and survives every cut in the roster.
- **The group spend key** (Ed25519 point, prime order). Minted by the DKG;
  **nobody holds the private scalar**. Each member holds a threshold share
  (`ThresholdKeys<Ed25519>`); *m* shares sign, *m − 1* reveal nothing.
- **The shared private view key** (one scalar, held by every member). **Not
  produced by the DKG** — no library in the stack derives one (verified
  2026-10-06). It comes from a contribution round inside the run (§3.4).
  Every member sees the treasury's balance and history; outsiders see
  nothing. This includes agent seats: whoever operates an agent sees the
  treasury. The consent prompt says so.

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
proposer          every member                          chain
   │ WalletInit       │                                   │
   ├─ propose {wallet_init, birthday} ────────────────────▶│ sealed at m
   │                  │                                   │ (feature on)
   │  starter: run r  │                                   │
   │         ┌─ start + readiness/consent (all n online) ─│
   │         ├─ round 1: commitments + PoP + view contribution
   │         ├─ round 2: encrypted shares + transcript hash
   │         ├─ local completion: keys, view key, address
   │         ├─ persist keys file, THEN broadcast attestation
   │         └─ n attestations collected                  │
   ├─ propose {wallet_created, …, attestations × n} ──────▶│ sealed at m
   │                  │          scanners start            │
```

Two layers, deliberately apart:

- **The init** is one ordinary m-of-n vote on the chain. It turns the
  feature on and fixes the birthday. It happens **once** per republic.
- **A run** is one attempt at the DKG. It is ephemeral (control frames
  only), carries a fresh 32-byte **run nonce**, and needs all n online at
  once. A failed run leaves no trace; the next one is a new nonce, never a
  resumption (I5), and costs no block.

### 3.1 The door: the init vote

`WalletInit` proposes `{op: wallet_init, birthday_height, network}` on the
Wallet surface. The engine refuses unless:

- the founding rule satisfies `2 ≤ m ≤ n − 1` (W1, from the genesis or
  anchor `rule_m`/`rule_n`, never the local config);
- the republic is chain-governed;
- no `wallet_init` is applied yet and none is pending.

The init is the only door (W4). `wallet_init` may be proposed while the
`wallet` feature is off; once applied, `wallet` counts as an effective
feature. A `set_features` vote may carry `wallet` only as the enable-only
ride-along of an already effective feature; adding it is refused with one
line pointing at the purse. A legacy `wallet` feature (old wizard default,
MCP founding, earlier vote) is therefore just a feature: its republic gets
the same "Set up the purse" action and nothing happens without it.

**Birthday.** The proposer probes its daemon and proposes its height minus
a small margin. Every approver compares the birthday to its own daemon: it
must not lie more than a small slack above its own height (daemons lag
each other by a block or two), nor more than a fixed window below it. A
lying or forked daemon could otherwise set a future birthday and make
every scanner miss deposits. A seat without a daemon **abstains** — it
neither approves nor declines — and approves once one is set.

**Consent** is per seat and explicit: approving the init card is consent
while this seat's log still carries the approval (compaction or a restore
can drop it; the run then asks again). A seat that never approved is asked
in the run (§3.3). A decline kills the card on every seat that has seen it
(every signing path, not only `approve`); m approvals made before the
decline arrives can still seal it, so the guarantee of W6 is the run's
consent, not the card.

**The card is reachable while the feature is off.** `wallet_init` is exempt
from the feature gate at propose, approve and re-sign, and the wallet
entry appears in the navigation as soon as an init is pending or applied.
On a later enable the purse panel shows the card itself.

The projection takes the **first** applied `wallet_init` and ignores any
later one; the closed op set (§3.6) keeps a second from being proposed.

### 3.2 When the init is proposed

- **At founding (W9).** The wizard's feature step offers the wallet,
  **checked by default** when W1 holds; otherwise locked off with the
  reason. The checkbox puts `wallet` into the ratified charter features
  (roster-v5, unchanged bytes), next to the one-line statements of §3.3.
  **The purse stage is the wizard's last step**, for the founder and
  every joiner: after the seal the wizard moves to it on its own. It first
  asks for a daemon where none is set (§7), then shows the progress bar
  (§3.4). Once the founding of a **create or join** finishes (never a
  recovery, never a reopen), the lowest founding position **that has a
  daemon** proposes the init; the next position follows after a fixed
  delay if no init appears. Every seat that ratified `wallet` **in this
  founding session** auto-approves after the birthday check — it consented
  by ratifying. "Enter republic" is available from the start of the stage;
  leaving it moves the bar to the purse panel. Without `wallet` in the
  charter the wizard ends at the seal as today. The founding itself never
  waits for the purse: the republic is sealed before the stage begins, and
  an aborted stage leaves it founded with the "Set up the purse" action.
- **Later (W10).** The organization dialog's wallet checkbox, and the
  wallet view's "Set up the purse" in a republic without a purse, both
  propose the init. When it is applied, the run starts as below and every
  seat gets the purse stage as a panel over the main window, with the same
  progress bar. All n must be online at once; the stage says so in one
  line.

### 3.3 Starting a run

The **starter** is founding position 1; if no start frame for the applied
init arrives within a fixed delay, position 2, and so on. It starts a run
automatically **once per init**, when the projection's first init is
applied in this session (a start frame for an init the others do not hold
yet waits in the readiness deadline). Every later run is started by a
member's explicit "Try again".

1. **Start** (broadcast): `init_id`, a fresh run nonce `r`. A nonce this
   seat has seen before is refused. Two starts racing in readiness resolve
   deterministically: every seat joins the one with the lowest starter
   position, then the lowest nonce. A seat may join a new run while it
   holds a keys record of an earlier one: it keeps that record (§3.5), so
   if the earlier run still completes, it simply wins.
2. **Readiness and consent.** Each seat announces a wallet-capable build, a
   reachable daemon, and its consent. A seat without consent gets the
   prompt (one line each: every member sees the balance; a share lost
   without a backup is gone; spending does not work yet). A decline is
   broadcast and aborts the run (W6). Missing announcements by the
   deadline abort without blame; the stage names who was missing.

A crash, timeout, decline or abort before round 2 ends the run with no
trace. A restart never revives a run: on reopen, a seat with neither run
state nor a keys record for an unfinished run aborts it; a seat **with** a
keys record never aborts that run, it re-attests (§3.5). Only a new start
frame begins another run.

### 3.4 The rounds

The rounds travel as **control frames** over the MLS group, resent on the
presence tick until the purse commits or the run aborts. They never enter
the workspace log, so "no trace" holds and older builds are not fed
unknown events.

1. **Round 1** (broadcast): PedPoP commitments + proof of possession, and a
   fresh 32-byte **view contribution** `c_i`. Commitments with the identity
   point are rejected before they reach the library.
2. **Round 2**: per-recipient secret shares (encrypted by the DKG protocol),
   plus the sender's **transcript hash** `T = H("molt-wallet-transcript-v1"
   ‖ r ‖ every round-1 message in participant order)`. A seat that sees a
   different `T` from anyone aborts.
3. **Local completion.** Each node derives its `ThresholdKeys` (identical
   group key everywhere), the view key, and the **standard** main address
   from (group key, view key).

**Progress (W9, W10).** Every seat shows the same stages from what it has
received: seats ready `k/n` → round 1 `k/n` → round 2 `k/n` →
attestations `k/n` → sealed. On abort: the one reason, and "Try again".
The engine computes it (MCP sees the same), the GUI only renders it.

**Bindings, all length-prefixed and entry-counted:**

- PedPoP context: `H("molt-wallet-dkg-v1" ‖ republic_id ‖ init_id ‖ r ‖ t ‖
  n)` (the library asks for a context unique per multisig; the nonce makes
  it unique per run).
- `view = H_s("molt-wallet-view-v1" ‖ republic_id ‖ init_id ‖ r ‖ (i ‖ c_i)
  for i in participant order)`.
- Integer and id encodings are fixed and pinned by byte-pin tests.

**Why the transcript hash.** PedPoP does not detect a sender who hands
different commitments to different parties ("responsibility lies with the
caller"), and requires confirming successful completion with all
participants. MLS authenticates senders but does not guarantee every member
saw the same message (a sender can publish two different events to disjoint
relays). `T` in round 2 and in every attestation closes both gaps; a
second, differing round-1 frame from one sender is equivocation and aborts.

**No public blame.** An abort names its reason, never a culprit: frames are
MLS-authenticated, not signed, so a third party could not check an
accusation. The local log records the MLS sender and the library's blame
result for the operator.

**Bias.** The last contributor can choose `c_i` after seeing the others.
That is harmless: the hash makes no chosen view key reachable, and the view
key's secrecy rests on the MLS group's confidentiality, not on randomness
against members — every member holds it anyway.

Round state cannot be persisted (the library's machines have no
serialization). **After** a seat sent round 2, it keeps its state until the
purse commits or the run aborts; once it persisted, the keys record carries
everything the run still needs from it.

### 3.5 Persist, then attest; the self-proving record (W8)

Each node writes a **keys record** for this run (share, view key, init id,
run nonce, `T`, address, network, `m`, `n`, birthday, and its own
attestation) through the blocking writer **before** the attestation leaves
it. Then it broadcasts its **attestation**:

```
A_i = Ed25519-sign(identity_sk_i, "molt-wallet-attest-v1"
      ‖ republic_id ‖ init_id ‖ r ‖ T ‖ address ‖ network ‖ m ‖ n ‖ birthday)
```

(length-prefixed, pinned by a byte-pin test; its own domain tag. The
identity key also signs MLS content, so a test pins that no OpenMLS
sign-content prefix can collide with the tag.) Any seat holding
all n valid attestations proposes the terminal change — founding position
1 first, the next position after a fixed delay:

```json
{"op": "wallet_created", "init": <init id>, "run": "<hex r>",
 "transcript": "<hex T>", "address": "<standard main address>",
 "network": "mainnet", "threshold": <rule_m>, "participants": <n>,
 "birthday_height": <h>, "attestations": ["<hex sig>", … n, participant order]}
```

- **It seals at m**, like any vote. Every approver co-signs only an exact
  match with its own computation and only with n valid attestations.
- **The projection proves all-n from the payload alone.** The purse is the
  first applied `wallet_created` that references the applied init, in a
  republic whose genesis satisfies W1, carries the init's `birthday_height`
  and `network`, `threshold == rule_m` and `participants == n` of the
  genesis, and n
  attestations that each verify under the identity key of founding
  position `i`. Everything it reads — the Wallet entries in block order,
  `rule_m`/`rule_n`, the founding identities — survives a checkpoint cut,
  so a seat that rejoined from an anchor computes the same purse as one
  holding the full chain. m colluding seats cannot seal a purse of their
  own: they lack the other attestations.
- **First wins, siblings die.** Later `wallet_created` entries are ignored;
  on the commit every sibling card is settled (as stale vault cards are),
  so no re-sign revives one.
- A seat whose computation differs never attests; the run cannot complete.
- **Restart between attest and seal:** on reopen the engine finds its keys
  records, re-broadcasts each attestation, and co-signs a matching pending
  `wallet_created`.
- **A keys record is never replaced before the purse is final.** A seat
  keeps one record per run it attested; any run that gathers n
  attestations is therefore one whose share every seat still holds, even
  if an attestation was withheld and released later. The records of
  every other run are deleted once the purse block is final (below a
  checkpoint cut, where no reorg reaches): until then a re-base may make
  another run the purse.

### 3.6 What chain verification does NOT do

The wallet adds **no rejection rule to chain verification** (I13). Older
builds accept any Wallet op, legacy republics already carry mock Wallet
blocks (the old `transfer` op), and verification is all-or-nothing: a new
rule would reject existing chains or fork a republic the moment an older
seat seals something. Instead:

- the closed op set (`wallet_init`, `wallet_created`) is enforced at
  propose, approve, wire ingest and in every signing path;
- the projection deterministically **ignores** any other Wallet op, every
  `wallet_init` but the first, and every `wallet_created` that is not the
  first valid one (§3.5).

## 4. What the chain learns — and what it never does

On the chain: address, network, transcript hash, run nonce, threshold,
participants, birthday, init id, and n attestations. The address carries
the view **public** key, so a seat that later receives the view key checks
`view·G` against it — no separate commitment is needed. Never on the chain
or in any log: the view key, any share or `ThresholdKeys` material.

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
  pass decides whether it gets a governed path. Until then W11's warning
  stands as written.

## 6. Persistence and backup

Two files, because one secret must never be lost and the rest is cheap:

- **Keys file** — the keys records (§3.5): one per attested run until the
  purse is final, then exactly the purse's. Written through the blocking
  writer; a record is never replaced. Encrypted on the
  `chain.state` pattern (own HKDF sub-key, own AAD segment, atomic replace,
  zeroized in memory).
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
- On import and on open, the record is checked against the projected
  purse (init, run, address). A record of another run makes the seat view
  only, loudly — a backup taken before the purse committed may hold one.
- A backup plus its secret is therefore a full share, as it already is a
  full seat. The backup dialog says so.
- Version skew: an older build exports without the keys file (it does not
  know it) and refuses to import a blob that carries it. Release notes say
  so.

## 7. Watching the purse

- **The warning (W11).** Wherever the deposit address is shown (wallet
  view, receive, the end of the purse stage), directly beside it and in
  large letters: **"Spending does not work yet. Money sent here is gone."**
  Not dismissable while Stage 2 is unbuilt. On stagenet/testnet the same
  line, since the habit is what matters.
- **Standard scanning.** A standard main address and the library's ordinary
  `Scanner` with a `ViewPair` — not the guaranteed scanner, whose featured
  addresses ordinary wallets do not pay correctly. The scanner keeps the
  **seen output keys** and ignores any repeat (documented as mandatory
  against the burning bug). It needs only the spend public key and the
  view key.
- **Main address only.** Subaddresses with multisig are unverified upstream.
- **Confirmations: 20.** Shown as pending below (the network saw an
  18-block reorg in September 2025); an output under an additional
  timelock (a mined output's 60 blocks, a sender's unlock time) stays
  pending until it unlocks. A reorg is detected against the stored
  recent block hashes; then the scan rewinds or rescans from the birthday.
- **The daemon** is the member's choice; **no daemon ships with the app**
  (as with relays: a default would be a default surveillance point). The
  purse stage asks for one when none is set, prefilled with a suggestion:
  every seat with a daemon announces its URL in a control frame while a
  purse stage is open (never on the chain). A seat that takes one trades
  some of the cross-check below for a click. It is reached through
  monero-daemon-rpc with a transport over the project's HTTP client and
  dialer. Its host is classified by the **same WHATWG-parsed host rule as a
  relay** (onion / local / clearnet), with `http`/`https` as the schemes
  (`http` only to an onion or local daemon, as `ws` for relays);
  userinfo in the URL is refused (login is its own setting). An onion
  daemon is dialed over Tor; a local or clearnet daemon rides the same gate
  as a local or clearnet relay — dialed only when non-onion dialing is on
  (`clearnet_enabled`) and the daemon was confirmed with the exposure
  acknowledgement. Response-size limits are per call.
- A lying daemon can hide funds from that member or lag; it cannot fake
  ownership. n independent scanners are the cross-check.
- **Network** (mainnet, stagenet, testnet) is a setting, **mainnet by
  default** (W11); every seat's must match the init's, and the purse
  records it.

**Usability.** The purse looks and reads like the official Monero GUI in
simple mode: balance, receive, history; the multi-party machinery stays
invisible (no DKG, round, share or view-key words in the UI). The rules
are in the plan, §11.

## 8. Stage 2 — spending (gated, not designed here)

Built only when all hold (W2):

1. monero-wallet ships **SA+L threshold signing** for the legacy key
   hierarchy, `SalLegacyAlgorithm` over `ThresholdKeys<Ed25519>` (today an
   unreleased branch, not wired into the wallet crate; the Carrot-native
   `SalAlgorithm` runs over `Ed25519T` and does not fit this DKG's key);
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

- Same hard fork: v17 transition, v18 final. No mainnet date (2026-10-06;
  the stressnet fork ran 2026-10-05).
- **Keys and address survive.** Carrot keeps existing addresses; the
  legacy-hierarchy structure (DKG spend key + shared view key) stays valid.
- **The scanner does not.** monero-wallet 0.2.0's scanner refuses blocks
  above hard-fork version 16 (`UnsupportedProtocol`), and its decoder fails
  on unknown output types. The scanner treats `UnsupportedProtocol` as
  **"scanning paused: update needed"** and a decode error as a daemon fault
  (`scan_paused = "daemon fault"`, never the pause: otherwise a lying
  daemon could fake it). Funds are safe meanwhile;
  only the display stops. The scanner sits behind the treasury's own
  interface, so the Carrot upgrade swaps an implementation.
- **Spending after v18** needs SA+L (§8). CLSAG transactions become
  invalid; this is why W2 waits.

## 10. Load-bearing invariants — do not weaken

- **I1 One threshold.** Spend authority is `rule_m`.
- **I2 Sign-what-you-see.** A seat attests and co-signs `wallet_created`
  only over values its own DKG computed, including the transcript.
- **I3 Secrets never touch the chain or the log.** View key, shares.
- **I4 Persisted before attested.** The keys file is on disk before the
  seat's attestation exists.
- **I5 One-shot run.** Never resumed; a failure means a new run nonce.
- **I6 All n.** A purse exists only with n valid attestations, checked by
  the projection from the payload; one decline kills the run.
- **I7 The group key is DKG-born.** Never derived, never assembled — the
  emergency exit (§5) is the one recorded exception, not built.
- **I8 A share exists exactly once** — in its member's keys file and that
  member's own backup. No escrow; the vault must not hold wallet shares.
- **I9 Library discipline.** Unique DKG context per run; identity points
  rejected; transcript confirmed with all participants before completion.
- **I10 Main address only.**
- **I11 Deterministic sealed values.** Participant order, birthday, block
  fields come from ratified intent or DKG output.
- **I12 Never lose a share silently.**
- **I13 No new verification rule.** The wallet's rules live in propose,
  approve, ingest, signing and projection — never in chain verification.
- **I14 Only the init opens the door.** No feature vote, legacy
  enablement, or inferred marker creates a purse.
- **I15 The founding never waits for the purse.** The stage begins after
  the seal, "Enter republic" is always open, and a failure leaves a
  founded republic. The founding ritual's code and bytes are untouched.
- **I16 A keys record is never replaced** before the purse is final.

## 11. Failure and abort

| failure | behaviour |
|---|---|
| bounds, not chain-governed, init exists | `WalletInit` refused, naming the reason |
| `set_features` adds `wallet` | refused, pointing at the purse |
| birthday outside the approver's window | that seat declines; the card dies where the decline is seen |
| a seat has no daemon | it abstains until one is set |
| a seat declines consent | run aborted; "Try again" |
| a seat not ready or offline | abort at the deadline, missing seats named |
| timeout before round 2 | abort, no blame, no trace |
| equivocation (two round-1 messages, differing transcripts) | abort; reason only, culprit in the local log |
| invalid share or identity point | abort; reason only, library blame in the local log |
| crash before persist | that run is aborted on reopen; "Try again" |
| crash after persist, before seal | reopen re-broadcasts the attestation and co-signs; never aborts that run |
| keys record of another run than the purse | view only, loudly |
| a seat computed a different result | it never attests; the run cannot complete |
| `wallet_created` without n valid attestations | ignored by the projection |
| daemon unreachable | status shows it; scanning resumes later |
| fork reached, scanner outdated | scanning paused, loud, funds safe |
| keys file damaged at export | export fails; acknowledge to set it aside, then watch-only |
| keys file damaged at import | restore without it, watch-only |
| scan file damaged | rescan from the birthday |

## 12. Dependencies and threat notes

- **Stage 1 stack** (MIT, pure Rust, locked in `molt-treasury` 2026-10-09,
  no `ring`, no C, no `*-sys`, pinned by `tests/graph_guard.rs`): `dkg`
  0.6.1, `dkg-pedpop` =0.6.0,
  `dalek-ff-group` 0.5 + `ciphersuite` 0.4 (the Ed25519 suite),
  `monero-wallet` 0.2.0 **without** the `multisig` feature,
  `monero-daemon-rpc` 0.2.0. Stage 1 needs neither `modular-frost` nor
  monero-wallet's `multisig` (outside SemVer); Stage 2 adds what SA+L
  requires.
- `monero-simple-request-rpc` is out: it pulls `ring` and cannot use the
  project's dialer.
- **`dkg-pedpop` is a dead end upstream.** Still on Serai's default branch,
  removed on the active `next` branch (2025-08-23); its successor
  `dkg-evrf` is not on crates.io. Its last audit (2023) covered older code.
- **Verdict (2026-10-09): crates.io, pinned `=0.6.0`, not vendored.** The
  review of its source (`lib.rs`, `encryption.rs`) found no defect to
  patch, only caller duties; the lockfile checksum fixes the bytes and a
  yank cannot break a locked build. Vendoring would put 1.2k lines of
  crypto under our maintenance with no change to make. Revisit on a
  needed fix or the move to `dkg-evrf`. What the review found:
  - *Context* enters the PoK challenge, the share cipher, the per-message
    PoP and the DLEq transcript (labeled `flexible-transcript`, distinct
    DSTs), so a frame from another run fails (pinned by a test).
  - *Identity points:* `read_G` refuses non-canonical and torsioned points
    but accepts the identity. An identity `A_0` passes the PoK (its log is
    known); an identity encryption key or per-message key makes that share's
    cipher key public to every group member. The wrapper must reject the
    identity in **every** point of both frames (commitments, PoK nonce,
    encryption key; round 2's message key and PoP nonce), read at their
    fixed offsets since the frames expose no accessors (`dkg::round2`,
    `dkg::complete`). The group key is refused too.
  - *Equivocation* is the caller's (the transcript hash, §3.4).
  - *Panics* (release is `panic = "abort"`): the DKG path has none reachable
    after `validate_map`; the blame path indexes by participant
    (`enc_keys[&decryptor]`, `commitments[&sender]`) and panics on an index
    outside 1..=n. The engine logs the participant from `InvalidShare` and
    never feeds wire indexes to `blame` or `AdditionalBlameMachine`.
  - `PedPoPError<Ed25519>` has no `Display` (a derive bound), so errors are
    debug-formatted into the local log.
- **Daemon login: `http-auth` 0.1.10** (MIT/Apache-2.0, 18M downloads,
  RFC 2617/7616 challenge parser and Digest client), with
  `default-features = false, features = ["digest-scheme"]`: md-5, sha2,
  hex, rand 0.8, memchr, all pure Rust, no `ring`, no `cc` (scratch build
  2026-10-09, monerod's MD5 `qop=auth` answered). Left: `digest_auth` 0.3
  (MIT only, drags `http` 0.2, last release 2023, no challenge-list
  parser).
- **QR: `qrcode` 0.14** (MIT/Apache-2.0, 22M downloads) with
  `default-features = false`: zero dependencies; the default `image` feature
  is off. `QrCode::new(uri)` → `to_colors()` + `width()` is the module grid
  the UI scales into a Slint `SharedPixelBuffer`. Left: `qrcodegen` (no
  release since 2022), `fast_qr` (MIT only, image path through `resvg`).
- **The `schnorr-signatures = "=0.5.2"` pin is load-bearing.** 0.5.3 is the
  same code but widens its `multiexp` range, which puts `multiexp` 0.4 and
  0.5 side by side and breaks `dkg-pedpop` (two `BatchVerifier` types).
  Do not "tidy" the pin away.
- **monero-oxide's bug bounty is paused.** Pin minors, upgrade deliberately.
- **What an attacker gets** from one member's device: one share (< m: no
  spend), the view key (privacy loss toward that attacker), a vote, and the
  ability to refuse attestation (no purse forms). From a daemon: traffic
  analysis, lag, at most a refused birthday. From m devices: everything
  governance gives — but not a purse of their own (I6).

## 13. What changed against rev 2

- The purse record carries n identity attestations; the projection checks
  all-n from the payload, so the purse survives checkpoint cuts identically
  for full-chain and anchor seats. Init and purse seal at the ordinary m.
- The init vote on the Wallet surface is the only door; the `wallet_rev`
  marker on `set_features` is gone, and `set_features` cannot add `wallet`.
- Init (once, on chain) and run (ephemeral, nonce, retry without a block)
  are separate; the starter and proposer fall back by founding position;
  racing starts resolve by position, then nonce.
- One keys record per attested run, never replaced before the purse
  commits; it carries everything a restart needs to re-attest.
- Daemon first in the purse stage; no daemon = abstain; birthday margin
  and slack; mainnet by default.
- Founding wizard: wallet checked by default when W1 holds; the purse
  stage with its progress bar is the automatic last step; the same stage
  as a panel for a later enable.
- No public blame; reasons only.
- Daemon classified by the relay host rule with `http`/`https`, behind the
  same non-onion gate; no default daemon.
- Mainnet allowed with the large warning next to the address.
- Upstream facts corrected: the pin's real cause, `dkg-pedpop` removed on
  `next`, Stage 2 targets `SalLegacyAlgorithm`, the scanner is
  monero-wallet's.
