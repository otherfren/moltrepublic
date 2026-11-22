# Chain divergence and failed rejoin: analysis and test plan

**Status: EXECUTED (archived 2026-11-22). Findings:
`docs_archive/chain/rejoin_divergence_findings.md`; §7 was built as
`docs_archive/chain/rejoin_divergence_build_plan.md` (v0.1.0) except the
rescue F-X1, which is open in `docs/chain/chain_rescue_plan.md`.** Successor of `docs/chain/chain_divergence_plan.md` (S1-S5 built
in v0.0.18, S6 open). Its fix candidates (§7) become a build plan only
after the analysis confirms them.

A live 2-of-3 republic diverged under v0.0.17. v0.0.18-v0.0.20 were
attempts to heal it and to bring a seat back by rejoin; none worked and
the republic was re-founded. Field copies of all three seats exist
outside the tree.

## 1. Questions this plan answers

- Q1: when, how and why did the chains diverge?
- Q2: why did the healing built in v0.0.18 not converge them?
- Q3: why did every rejoin fail?
- Q4: what prevents divergence reliably?
- Q5: what makes the rejoin work, also against a diverged republic?
- Q6: both fixes without losing backward compatibility.

## 2. Rules for the whole run

- **Originals are never opened.** `open_workspace` writes (lock file,
  tail truncation, segment sweep) and CREATES a random device key when
  none is found. Every step works on a fresh copy; a checksum manifest of
  the originals is taken first and compared after every phase.
- **Nothing from the field lands in the tree.** Captures, dumps, logs,
  phrases and the relay address stay under `$FIELD_OUT` (outside the
  repo). Docs record counts and shapes only; seats are A, B, C; the relay
  is `$TEST_RELAY`. Test fixtures are invented.
- **A copy that goes online speaks as the real seat** (same identity key,
  transport key, MLS ratchet, envelope seqs). Precondition for every
  online step: no real node has the old republic open (§9 Q-b).
- **One window-scale build at a time**; the field binary (v0.0.20) is
  used as delivered for reproduction, diagnostic builds come from a clean
  checkout of the same tag.

## 3. Inputs

| input | state |
|---|---|
| workspace copies, 3 seats | present |
| device key | 2 of 3 present |
| moltd tracing logs of the 3 nodes (7 daily files kept) | missing; expiring |
| field binary v0.0.20 + config | present |
| `$TEST_RELAY` (onion, already in the republic's pool) | reachable via local Tor SOCKS, to verify |

A seat without device key or phrase stays closed: its chain cannot be
read and it cannot be instantiated.

## 4. Phase T: theory (code only, no field data)

T1-T3 are already drafted from code reading (2026-11-21); each row is a
hypothesis until Phase O or L confirms it.

### T1. How forks still arise at HEAD

| # | mechanism | where |
|---|---|---|
| D1 | The sealing line is built per PROPOSAL from the seats that signed it, so two proposals have two rank-0 sealers at one height. The planned "smallest id with m signatures" rule is not built. | `governance.rs::sealer_turn`, `try_commit` |
| D2 | After a seal the node records its re-signed approvals for h+1 BEFORE the block at h; peers drop them as "implausible future height" and build a line without that seat. | `adopt_committed_block`, `rebase_pending_approvals` |
| D3 | Presence is a local view (120 s); two seats that miss each other are both rank 0. | `unseen_seats` |
| D4 | A fallback seals after k x 60 + 30 s although the leader's block is only late (Tor, relay paging, ordered park up to 900 s). | `sealer_turn`, `net/delivery.rs` |
| D5 | Approvals do not cover `prev` and every seat re-signs every open approval after each head move: both branches of a fork keep collecting m signatures and keep growing. | `approval_bytes`, `rebase_pending_approvals` |
| D6 | A cut is co-signed across an unhealed attestation variant (equal state hash, different `prev`), giving two anchor blocks at one height. | `checkpoint.rs` |

### T2. Where healing stalls

| # | state | effect |
|---|---|---|
| H1 | fork point at or below a seat's anchor | permanent: `try_reorg` never crosses the cut, the server can only send the blob, the blob is ignored unless head < `upto` |
| H2 | two anchor blocks at one height (D6) | permanent, and not even flagged; every retry re-broadcasts suffix + open governance |
| H3 | a cut sealed on the losing branch only | permanent for its holder |
| H4 | the winning branch fails verify on this node | loops |
| H5 | the diverged peer counts as silent | heal tick skips it; beacon only |
| H6 | three branches: one candidate slot per height, "kept" clears every peer's candidates | waits for re-serves (60-600 s) |
| H7 | displaced `Membership`/`Checkpoint` blocks never return to the vote; a tip-swapped card is not re-signed | decision lost until operator or restart |
| H8 | `peers_ahead` never decreases and counts abandoned branches | human votes refused |
| H9 | a relay stuck three paging rounds is not read again; one over-full second is skipped | silent loss until restart |

### T3. Where the rejoin fails

| # | mechanism | where |
|---|---|---|
| R1 | The rejoiner keeps the FIRST block per height from any sender and verifies the whole consecutive run, never the anchor alone. A readable block of another branch poisons the run for good. With forked survivors this makes the divergence itself the cause of the failed rejoin. | `nostr_ritual.rs::recovery_rejoin` |
| R2 | One absolute 15 min deadline from start covers vote, seal, re-key and anchor transfer; the anchor travels behind the coordinator's outbox; the re-push fires every 10 min. | `recovery.rs::RECOVERY_WELCOME_TIMEOUT`, `sync.rs::repush_unfinished_restores` |
| R3 | Frames at an MLS epoch ahead of the rejoiner are dropped, not held. | `recovery_rejoin` |
| R4 | The re-key removes the pending entry first; any early return after it leaves a sealed `Restored` block and no Welcome, with no retry. | `membership.rs::coordinator_rekey` |
| R5 | A request while a card or pending entry exists is dropped silently; a v1 pending entry is never swept. | `net/recovery.rs`, `membership.rs` |
| R6 | A v1 consent (any seat not v2-capable, the lost one included) is never auto-signed and not co-signed by the coordinator. | `membership.rs::consent_anchor_for` |
| R7 | The anchor is pushed, never pulled: the rejoiner cannot ask for it; parts are not owed by the delivery guarantee. | `sync.rs::serve_chain_anchor`, `blob_parts.rs` |
| R8 | A full part frame is ~66 KB on the wire, above a 64 KiB relay event limit. | `blob_parts.rs` |
| R9 | A `Restored` block on a branch that later loses disappears; the MLS re-key stays. | `sync.rs::try_reorg` |
| R10 | The first-in-line seat re-pushes its OWN anchor; if it is the diverged one, the rejoiner materializes on the wrong branch. | `repush_unfinished_restores` |

### T4. Still to do in theory

- Inventory of every fixed deadline in founding, join and rejoin (32
  duration constants in the ritual files alone), each classified: absolute
  give-up (to remove, §7 F-R2), retry interval (stays), debounce (stays).
- Per D/H/R row: the log line and the persisted trace that confirms or
  refutes it (table in the findings doc).
- Red tests with invented fixtures, loopback or `MockRelay`, one per
  confirmed row; minimum set:
  - rejoin while the survivors are forked (R1);
  - rejoin whose anchor arrives after 20 min (R2);
  - parts bootstrap end to end over a relay (promised by S3, not built);
  - two anchors at one height (H2);
  - `Restored` on the losing branch (R9);
  - three seats, bursts, 10 % dropped frames: one head hash (the
    acceptance test of the previous plan, not built; the existing one is
    two seats without drops).

## 5. Phase O: offline forensics on copies (no network)

**O0. Setup.** Checksum manifest of the originals. Per seat a sandbox
`<run>/<seat>/{device.key, workspaces/<dir>}`; the device key is placed
BEFORE the first open.

**O1. Tool.** A dev-only example `chain_forensics` (`molt-engine`
examples) on the public API: `molt_storage::open_workspace`,
`OpenedWorkspace::read_chain`, `read_log_from`, `block_link_bytes` +
`content_hash`, `verify_chain`. Output per seat, to `$FIELD_OUT`:
- chain: height, hash, prev, kind, proposal id, signer set, ts; anchor
  height + hash; blob bytes and state hash;
- own-envelope timeline from the event log: own `Committed` (including
  blocks later re-based away), own `Approved` (id, height), own
  `ChainRequest` with `known` (a head-hash sample roughly every 5 min),
  membership and recovery events;
- snapshot: open proposals, votes, `own_approvals`.

The tool is tested first against a loopback-founded republic (invented
data), then run on the copies.

**O2. Cross-check.** Each copy also starts once with the field binary
and an EMPTY relay pool (nothing is dialled; verified by a closed-port
check), read over MCP: `read_chain`, `status.chain_sync`. Must equal O1.

**O3. Comparison (answers Q1, Q2).**
- first height where two seats differ; classification: attestation
  variant, real fork, different anchor;
- do the three anchors agree (H1-H3)?
- fork count and depth over time from the `known` samples; which seat
  sealed which side; which version ran when (from the tracing logs);
- per fork: which of D1-D6 fits the signer sets and timestamps;
- which of H1-H9 holds in the final state.

**O4. Tracing logs**, once supplied: the strings of the previous plan's
S1 (`fork_decision`, `chain_catchup=…`, `chain_heal_stuck`, `anchor=…`,
`relay replay …`) and the rejoin lines (`rejoin_step`, `rejoin_frame`,
`recovery_failed`, the dropped-request lines) as counts per day and per
outcome; the final `the chain anchor did not verify …` reason of each
rejoin attempt decides between R1, R2, R3 and R8.

## 6. Phase L: live reproduction against `$TEST_RELAY`

Every run: fresh copies, pool reduced to `$TEST_RELAY` (the group dial
set is the ratified list intersected with the own confirmed pool, so
nothing else is dialled), local Tor SOCKS, headless, driven and read
over MCP, tracing at debug into `$FIELD_OUT/<run>/`. A run records start
state, every `fork_decision`, head hash per seat per minute, and its
verdict.

Relay residue: each run leaves events of the same seats on the relay,
and a fresh copy re-reads them (48 h overlap). Runs are therefore
ordered from least to most writing, and §9 Q-c decides how residue is
cleared between runs.

| run | seats online | question |
|---|---|---|
| L0 | each seat alone | does the copy open, reach the relay, replay within the bound? |
| L1 | each pair | do two seats converge on one head hash unattended? if not: which H row? |
| L2 | all available | the same for the full republic |
| L3 | pair, then fresh rejoiner for the third seat (phrase pulled from its copy by `reveal_seed`, kept in memory only) | reproduce the failed rejoin on v0.0.20; the final reason names the R row |
| L4 | as L3, each seat in turn as coordinator, the other survivor up / down | which role distribution fails |
| L5 | L1-L4 on a HEAD build | what the two post-release fixes changed |
| L6 | L1-L4 on the fixed build (§7), mixed with one seat on v0.0.20 | acceptance and backward compatibility |

Pass for L6: one head hash on all seats without restart; the rejoiner
materializes, catches up to that head and its next proposal seals; the
v0.0.20 seat neither stops nor forks off.

## 7. Fix candidates (to be confirmed, then planned in detail)

Compatibility rules for all of them: no signed layout changes
(`approval_bytes`, block link, roster, checkpoint keep their tags and
byte-pin tests); new wire fields and events are additive
(`#[serde(default)]`, unknown variant = dropped frame, proven with the
old deserializer); stricter behaviour activates only once every roster
seat advertises it; a v0.0.17-shaped chain and a diverged one must open
and heal.

**Prevent divergence**
- F-D1 One sealing line per HEIGHT, and one deterministic choice (the
  smallest id holding m signatures). Local behaviour only.
- F-D2 Record the block before the re-signed approvals; park a
  future-height approval instead of dropping it.
- F-D3 No cut while a peer is diverged or a variant is unhealed.
- F-D4 The Flexible-Paxos lock (S6 of the previous plan) as the only
  measure that makes forks impossible; capability-gated.

**Heal what exists**
- F-H1 The fork rule also decides at the anchor: a seat whose anchor
  loses re-anchors from the served blob (H1-H3).
- F-H2 Displaced `Membership` and `Checkpoint` blocks return to the
  vote; a tip swap re-signs (H7, R9).
- F-H3 `peers_ahead` ignores diverged peers; relay down reasons reach
  `status`; a stuck relay is read again after a backoff (H8, H9).

**Rejoin**
- F-R1 The rejoiner verifies the anchor alone, keeps blocks per (height,
  hash), and takes its anchor from the coordinator it addressed (R1,
  R10).
- F-R2 **No fixed deadlines** (user, 2026-11-21): every waiting step in
  founding, join and rejoin runs until abort or a definite failure, with
  a visible state line and bounded retries behind it; T4's inventory is
  the work list.
- F-R3 The anchor can be pulled: once welcomed, the rejoiner asks for it
  (additive request) until it verifies (R2, R7).
- F-R4 Frames ahead of the rejoiner's epoch are held (R3).
- F-R5 The re-key keeps its pending entry until the Welcome published
  and retries (R4); a request during a pending re-admission gets an
  answer (R5).
- F-R6 Parts sized under the smallest relay event limit in the pool
  (R8).
- F-R7 The request carries the rejoiner's version (additive).

**Added and re-ranked after the runs (findings §6)**

Order of value; 1-4 are local behaviour, no wire or layout change:

1. F-R8 The rejoiner reads while it waits: drain the subscription
   during the replay instead of waiting for its end (R11). Smallest
   change, removes the reproduced failure.
2. F-R9 An evicted node notices: a commit that removes the own leaf
   stops the outbox, shows "this seat was restored elsewhere" and logs
   at WARN; resends to a peer that never acks stop growing the relay
   (R12, N1).
3. F-R10 No restore of a live seat, no automatic one: self-heal never
   asks for a restore by itself; a coordinator refuses to mint or seal
   while the seat is heard, and while it knows a fork (N1, D-rows).
4. F-R11 Seal and re-key only after the rejoiner holds the anchor, or
   undo on failure: no block and no trusted key from a failed attempt
   (R13).
5. F-R2 no fixed deadlines, F-R3 anchor pull, F-R4, F-R5 as above.
6. F-H4 A `set_relays` change does not seal while a seat is known
   diverged, or the previous list stays readable for a transition (N6).
7. F-D1-F-D3, then F-D4 if forks remain.

F-R1 (poisoned run) drops to a hardening item: not observed.

**Rescue (candidate; built AFTER the fixes above, user 2026-11-21)**
- F-X1 A rejoin that can save a republic whose chains diverged and
  whose MLS group split: rejoiner and coordinator find the last block
  both hold (the `known` exchange), the rejoiner's consent names it, and
  the coordinator seals a rescue block right above it that discards
  everything higher. Sealed like `Restored` (one survivor plus the
  consent); further seats follow the same way.
- The split group cannot be rolled back (MLS deletes old epoch
  secrets), so the coordinator forms a FRESH group instead of
  re-keying.
- The cut is a block, never a silent truncation: signatures do not
  cover `prev`, so a seat that missed the rescue could re-offer its old
  branch; the rescue block takes precedence at its height.
- Bounds: never below the anchor; an explicit operator decision, never
  automatic. Displaced `Applied` decisions return to the vote,
  displaced `Restored` blocks are dropped.
- Compatibility: one additive `ChainChange` variant (an older build
  stops extending); every seat needs the new build.
- Size: about that of the recovery ritual; its own design doc first.
  The fresh-group path is the riskiest part.

## 8. Deliverables

1. `chain_forensics` example + its test.
2. Findings doc (`docs/chain/`): answers to Q1-Q3 as counts, shapes and
   a timeline; one verdict per D/H/R row.
3. The red tests of T4.
4. The build plan for §7, staged, with the mixed-version acceptance run
   L6; the rescue (F-X1) as a separate design doc and stage after it.

Order: O0-O3 -> T4 -> L0-L4 -> findings -> build plan -> build -> L5-L6.

## 9. Open questions

- Q-a The third seat's device key (or its phrase) and the tracing logs
  of all three nodes: obtainable?
- Q-b Is the old republic closed on every real node, for the whole
  online phase?
- Q-c DECIDED (user, 2026-11-21): repeat runs use a throwaway local
  relay; the diagnostic build carries a dev-only override of the
  ratified-list gate, never the release.
- Q-d DECIDED: the copies may write to `$TEST_RELAY`.
- Q-e DECIDED: analysis and solution design first; the solutions are
  explained to the user before any production code changes. Until then
  only dev-only tooling (examples, tests) is written.
