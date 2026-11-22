# The chain lock: wiring plan (release v0.1.1)

**Status: WIRED, OFF BY DEFAULT (2026-11-22), on a branch based on
master 0.1.0.** Design: `docs/chain/chain_lock_design.md` (§14 "As
built"). The lock runs only where `CHAIN_LOCK_DEFAULT_ON`
(`chain/lock_wire.rs`) or the test seam `__lock_cap_off(false)` turns it
on; off, a seat is B6 exactly. It stays off until activation is one
agreed height instead of a race (§5); §8 proposes how, for a decision.

## 1. What the lock is for

B6 gives one sealer per height and took the measured fork decisions in a
lossy three-seat test from 11-19 to 0, but a fallback sealer can still
seal a different proposal after a lost block. The lock makes two
different blocks at one height impossible among honest seats: a seat
signs a position only for the value its leader announced, and a new
leader must first learn what was accepted. Signed bytes do not change.

## 2. What exists

- Wire: five additive events (`LockPrepare`, `LockPromise`,
  `LockAccept`, `LockAccepted`, `LockNack`), not owed by the delivery
  guarantee, byte-pinned; a pin that an older build drops them; the
  capability `molt-chain-lock-v1`; `chain_sync` status fields.
- Storage: a sealed `chain_lock.state` with its own key, written
  blocking and atomically, excluded from exports.
- Engine (`chain/lock_wire.rs`): one gate for free signing, one for lock
  acceptance; every acceptor change is persisted before its frame and
  rolled back when the write fails; intents instead of position
  signatures at locked heights; leader, takeover, forced value, seal;
  lineage with a cache; the corrected release rule.
- Position signing has exactly two production call sites (§3).

## 3. Position-signing paths (re-derived on the merged tree)

Production `identity_sign` over `approval_bytes`: `governance.rs`
`chain_sign_and_gossip_approval` (behind `lock_sign_freely`) and
`lock_wire.rs` `lock_acceptor_accept` (behind `lock_step`). Everything
else (founding roster, recovery test fixtures, vault deposits) signs
other bytes or is test code. Callers of the first door:

| path | entry | at a locked height |
|---|---|---|
| approve | `proposals.rs` `cmd_approve` | intent |
| proposer's own approval | `proposals.rs` `propose_payload` | intent |
| membership propose / restore auto-approve | `membership.rs` `propose_membership_with`, `auto_approve_restore` | intent; the accept re-checks consent, `restore_gate`, `sign_blocker` |
| cut propose / co-sign | `checkpoint.rs` `cmd_propose_checkpoint`, `receive_checkpoint_proposal` | intent; the accept re-checks the hash |
| re-base after a head move | `governance.rs` `rebase_pending_approvals` | nothing re-signed (intents stand) |
| B5 held approvals after a heal | `governance.rs` `sign_held_approvals` | intent |
| reorg re-sign | `sync.rs` `try_reorg` | intent |
| wallet co-sign | `wallet.rs` `wallet_review`, `wallet_run.rs` `wallet_cosign_review` | intent; the accept re-checks |
| release re-sign | `lock_wire.rs` `lock_try_release` | free (the mark is written by the release) |

`sign_blocker` (B5 relay wait, evicted seat, chain behind) runs before
the mode gate, so a held approval consumes no free mark. The verified
park (`park_approval`) and relay approvals (`receive_approval_via`) only
receive; lock signatures never enter `pending_sigs`, so no re-serve can
turn one into an `Approved`.

## 4. Steps

1. Merge the WIP onto master, then master 0.1.0: done (all conflicts
   additive: dump fields, the seam beside `seal_first`, re-exports).
2. Re-derive the signing paths: done, re-checked after each merge (§3).
3. Review the release rule: a counterexample found and corrected (§5).
4. Suites with the lock off: green. With the lock on: §6 (13 vault
   tests stall on the activation race).
5. Lossy trio and lock integration tests, five runs each: done (§7).
6. Decide activation (§8): OPEN, the user's decision.
7. Build the chosen activation; re-run the lock-on suite (the 13 vault
   tests are its acceptance check).
8. Traffic measurement over Tor against a real relay: open.
9. Acceptance on field copies: open.
10. Switch the default on.

## 5. The release rule (review result)

The rule as first wired (promise-only state, `|F| >= n - m`) is safe but
not live: 2-of-2 and 3-of-3 stall for good when the leader already
accepted, 2-of-5 when two seats signed freely (phase 1 can never
complete, the rule waits for three). Pinned at seeds 2, 1, 0. Corrected:
release all lock state once `|F| >= min(m, n - m + a)`; proof in design
§14; 16 shapes x 800 seeds with a liveness phase pass.

Blocking the default: a card whose voices straddle a free seat and lock
seats (3-of-5: one free, two lock) waits for more lock voices where B6
would seal (design §14, pinned). Not rare: after a founding the first
approvals are free (no capability known yet), the next ones intents;
the vault suite (2-of-4) stalls at height 1 in every test that waits
for a commit. Proposed correction: replace the race by one agreed
activation height - the founding fixes it for a new republic (every
founder runs the build), a chain block names it for an existing one -
so no honest seat signs freely at a height another locks. The release
rule then only serves a seat restored onto an older build.

## 6. Suite with the lock on (the default flipped locally)

`molt-engine`: 1 316 passed, 32 failed before the fixes below.

| failures | cause | fix |
|---|---|---|
| 16 `nostr_recovery` | the restore's subject led the height (B6's line), the survivor waited a full grace | the lock line leaves it out; held restore seals retried |
| 1 of them, `a_bystander_that_slept_through_a_recovery_still_hears_the_rejoiner` | one survivor restores while the other sleeps: impossible under the lock by decision | runs on the capability-off seam |
| 13 vault (`vault_complaints` 6, `vault_deposit` 1, `vault_grant` 4, `vault_late_enable` 1, `vault_recovery` 1) | the straddle at height 1 (§5) | none: blocks the default |
| `a_reorg_keeps_the_approvals_it_releases_from_the_park` | intents displayed at a free height | intents count only at locked heights |
| `a_consented_restore_under_the_lock_needs_both_survivors` | the wait text names the missing seat now | expectation updated |

After the fixes: `nostr_recovery` 33/33; the 13 vault failures remain.
`checkpoint_under_load` (red in the first attempt) passes with the lock
on.

Also open: under the active lock a restore needs promises from
n - m + 1 seats, so in 2-of-3 BOTH survivors must be reachable to bring
the third back (the status shows `promises 1/2 missing <seat>`, the log
`lock=wait reason=restore_needs_promises`).

## 7. Measured (2026-11-22, loopback relay, 4-core box shared with a release build)

`chain_convergence.rs`, 2-of-3, two bursts of nine pages, 10 % inbound
loss, five runs each. Frames = every event the relay stored during the
bursts (votes, lock frames, blocks, acks, presence), per sealed block.

| | fork decisions | seconds | frames per block |
|---|---|---|---|
| lock | 0, 0, 0, 0, 0 | 93, 93, 78, 74, 83 | 24.0, 24.2, 23.3, 22.8, 23.7 |
| B6 | 0, 0, 0, 0, 0 | 111, 112, 110, 114, 123 | 55.5, 52.3, 50.8, 50.5, 50.4 |

The first attempt's estimate (about twice the frames) does not hold on
this relay: the lock sends fewer than half, because B6's re-signed
approvals and re-serves after every head move disappear. Not yet
measured over Tor against a real relay.

`chain_lock.rs` (7 tests), five runs: 7/7 in four; in one run, under
load, `a_capability_that_flips_mid_run_neither_stalls_nor_forks` found no
common head within 300 s. Cause: approvals cast as unsigned intents while
the lock ran were never signed when the height turned free (a B6 re-base
re-signs collected signatures only). Fixed: the tick signs standing
intents freely once per height that turns free; the flip test then
passed 12 of 12 under the same load (1 of 8 failed before).
The keystone (bursts, loss) had 0 fork decisions in all five runs,
63-85 s.

## 8. Proposal: one agreed activation height (for decision)

The race: whether a height is locked is each seat's own belief, built
from capability announcements that arrive at different times. Both
variants replace the belief by a fact every seat reads from the chain,
so no honest seat signs freely at a height another seat locks. Both use
the existing feature machinery (`docs_archive/ritual/charter_features.md`)
with one new key, `chain-lock`; no new layout, no tag bump - the key is
content inside bytes that already carry feature keys. Either way: the
lock is active at height h iff `chain-lock` is in the effective feature
set at h - 1; the capability only gates who may propose or ratify it.
D1 says feature keys are surface keys; `chain-lock` is the first that is
not, which the decision accepts or not.

### A. Fixed at the founding

- Wire: nothing new. The wizard offers `chain-lock` (checked by
  default); it rides the charter's feature list like `vault`.
- Signed bytes: the roster's feature list (roster-v5 layout, unchanged)
  names the key; the genesis is block 0 of a locked republic.
- Existing republics: cannot use A; they stay B6 (or use B).
- Older build: refuses to ratify a charter with a key it cannot render
  (`charter_features.md` §3) - an older member fails honestly at the
  founding, never mixed in.
  A seat later restored onto an older build reads the key (union fold)
  but runs B6; the release rule covers exactly that case.
- To build: the key in the feature vocabulary and the wizard;
  `lock_mode` reads the effective features instead of `seat_caps`; byte
  pins for a roster with the key; founding tests (`vault_support`
  founds with it).
- Vault tests: resolves all 13. Each founds a fresh 2-of-4 republic and
  proposes at once; locked from height 1, no seat signs freely, so the
  straddled first deposit cannot occur.

### B. Named by a chain block

- Wire: nothing new. A `set_features` vote adding `chain-lock`, proposed
  and approved only while every roster seat announced the capability.
- Signed bytes: an ordinary `Applied` set_features block (existing
  layout); the lock starts at the height after it. A cut carries the
  effective features already (checkpoint-v7), so pruned holders agree.
- Existing republics: one vote, m-of-n; after it every height is
  locked. Until then B6.
- Older build: the union fold keeps the unknown key (read-tolerant), it
  keeps signing freely - but none exists at the vote, by the gate. A
  seat restored onto an older build later is the release rule's case.
- To build: the key in `validate_org_payload` and the propose gate (all
  capable), `lock_mode` from the effective features, tests for the vote,
  a cut across it, a pruned holder, the old-build read.
- Vault tests: resolves none directly - they never vote, so they stay
  B6 and pass as with the lock off today. Combined with A (a founding
  sets it, a vote sets it for older republics) all 13 pass locked.

Recommendation: A and B together (A for new republics, B to upgrade
existing ones); the automatic capability race goes away, the release
rule stays only for a seat restored onto an older build.
