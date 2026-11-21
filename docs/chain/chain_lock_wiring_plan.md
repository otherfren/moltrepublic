# The chain lock: wiring plan (release v0.1.1)

**Status: PLAN (2026-11-21). Not in v0.1.0.** Design:
`docs/chain/chain_lock_design.md`. On master, unused: the pure state
machine `crates/molt-engine/src/chain/lock.rs` with its simulation. A
first wiring attempt was stopped by a plan change and is kept as two WIP
commits on the branch `wip/chain-lock-wiring` (base: the B5+B6 merge).
That branch has the lock ON and a red suite: it is material, not a
candidate.

## 1. What the lock is for

B6 gives one sealer per height and took the measured fork decisions in a
lossy three-seat test from 11-19 to 0, but a fallback sealer can still
seal a different proposal after a lost block. The lock makes two
different blocks at one height impossible among honest seats: a seat
signs a position only for the value its leader announced, and a new
leader must first learn what was accepted. Signed bytes do not change.

## 2. What the first attempt built (branch only)

- Wire: five additive events (`LockPrepare`, `LockPromise`,
  `LockAccept`, `LockAccepted`, `LockNack`), not owed by the delivery
  guarantee, byte-pinned; a pin that an older build drops them; the
  capability `molt-chain-lock-v1`; `chain_sync` status fields.
- Storage: a sealed `chain_lock.state` with its own key, written
  blocking and atomically, excluded from exports.
- Engine (`chain/lock_wire.rs`): one gate for free signing, one for lock
  acceptance; every acceptor change is persisted before its frame, and
  rolled back when the write fails; intents instead of position
  signatures at locked heights; leader, takeover, forced value, seal;
  lineage with a cache.
- Position signing has exactly two production call sites, the free
  approval and the lock acceptance; all paths (approve, proposer
  co-sign, re-base, reorg re-sign, held approvals, cut, restore,
  membership, wallet, vault) reach one of them through the mode gate.
- Tests: 15 wiring unit tests and 7 integration tests green once, among
  them three seats with bursts and loss at zero forks.

## 3. What the attempt found (design corrections)

| # | finding | resolution to carry over |
|---|---|---|
| 1 | **Activation can stall a height.** Capabilities arrive at different times; one seat already promised while another still signs freely. Safe, but in 2-of-2 neither path can decide. About 30 tests stalled. | A release rule: when at least n - m seats are known to have signed freely at the height, a seat holding only promises trades them, durably, for a free mark. Proven by a seeded simulation (releasing on the first free signer forks). To REVIEW, and preferably replaced by deterministic activation: the lock starts at a height named by a chain block. |
| 2 | "No lock file = amnesia" stalls every upgraded republic at once. | A marker in the workspace prefs says the lock ever ran here. Residual: an export from a pre-lock build imported after the lock ran. |
| 3 | A forced cut must wait for all n promises before it is re-proposed. | Built. |
| 4 | A promise must name the proposal id; the acceptor holds only a digest. | A digest -> (id, change) sidecar in the lock file. |
| 5 | Intents are lost across snapshots. | Dumped with the engine state (additive). |
| 6 | The reopen's re-base verdict dated cards by own signatures only. | Extended to intents; unverified. |
| 7 | A takeover fired on an idle height. | A non-default seat's clock starts once it holds a sealable value. |
| 8 | The rejoiner's progress list counts verified signatures only, so under the lock it shows voices after the seal. | Count intents in `recover_progress_for`. |
| 9 | 2-of-2 with an amnesiac seat waits until a block passes its floor. | Open (design question 4). |
| 10 | About twice the frames per block. | Measure against real relays before release. |

## 4. Steps

1. Rebase the branch onto master (B4 and the residual fixes touched the
   recovery files and `molt-core`); make it compile.
2. Advertise the capability only behind a default-off switch with a test
   seam: master behaviour stays B6 and the whole suite is green. Merge
   that state.
3. Decide activation (finding 1): review the release rule's proof, or
   build "active from the height a block names". Design first.
4. With the lock on in tests: triage the recovery set (9 failing, see
   finding 8) and the hard-kill re-base test (finding 6).
5. Each integration test five times; record timings and fork counts
   against B6 alone.
6. Traffic measurement over Tor against a real relay (finding 10).
7. Clippy 0 per crate; the GUI check through the live-preview chain;
   `molt-net` and `molt-mcp` suites.
8. Acceptance on the field copies: three seats on the lock under load,
   and one v0.1.0 seat mixed in (no stall, no extra fork, the lock stays
   off for those heights).
9. Switch the default on; release v0.1.1.

Open product question for step 9: under the active lock a restore needs
promises from n - m + 1 seats, so in 2-of-3 BOTH survivors must be
reachable to bring the third back. One survivor alone can no longer do
it; that case belongs to the rescue stage (analysis plan F-X1).
