# Chain divergence: diagnose, heal, prevent

**Status: PLAN, open work, not built (2026-11-20).** Execution-ready
stages S1-S5 and S7, one release; S6 is research only. Every question is
decided (§7). Threat model for this plan: **honest seats, faulty
network** (user, 2026-11-20). Verification stays as it is: `verify_chain`
hard-rejects, the hash rule (`docs_archive/chain/chain_reorg.md` R1)
alone decides a fork.

## 1. Observations (a live 2-of-3 republic, Nostr over Tor)

- O1: forks decided at 79 distinct heights over ~124 new blocks in one
  day; 3-23 fork decisions per hour; often 15+ open proposals.
- O2: after "keeping this branch at the fork - the peer re-bases", foreign
  blocks kept arriving for 20 minutes with no further fork decision; it
  healed only after restarts.
- O3: `checkpoint blob does not fit one frame - not serving it`
  (149 KB blob, 95 KB cap). Every seat holds the same anchor, so no seat
  can serve it; a sealed restore never completed, and the rejoiner's
  repeated requests were dropped ("reuses an anchored transport key").
- O4: six times a day `relay replayed more stored events than the bound`
  (bound 5000) on the same healthy relay; the node never read it again
  until restart. With a second relay dead, one relay was left.
- O5: `read_chain` has no block hash; operators hand-built fingerprints to
  compare chains.

## 2. Causes (verified in code)

| # | cause | where |
|---|---|---|
| C1 | Every seat re-signs EVERY open approval for head+1 after each head move, so with k open proposals each seat has legitimated k blocks per height. Forks are the normal case, not an accident. | `governance.rs::rebase_pending_approvals`, `try_commit` |
| C2 | Exactly m signatures in hand decide the attestation list, so the same decision gets different hashes on different nodes (attestation variant). | `try_commit` |
| C3 | A catch-up request for height f is suppressed while `catchup_from == Some(f)`; no timeout. It clears only when the head reaches f, on a reorg, or on open. | `sync.rs::request_catchup` |
| C4 | The fork winner clears its candidates and waits for the loser to ask. | `sync.rs::try_reorg` ("ours wins") |
| C5 | An over-cap blob is never served: neither for catch-up below the anchor nor in `anchor_bootstrap`, which silently omits it and sends a bare anchor block a rejoiner cannot verify. | `sync.rs::served_blob_fits`, `anchor_bootstrap` |
| C6 | The replay bound returns `ControlFlow::Break`, which ends the relay's supervisor for good. | `relay_runtime.rs::read_session`, `supervise` |
| C7 | The resume cursor advances to the newest `created_at` per event even before EOSE. Relays commonly replay newest first, so a session that dies mid-replay resumes past a gap it never read. | `relay_runtime.rs::read_session`, `place_req` |
| C8 | The fork-decision path "nothing links yet" logs nothing, so O2 cannot be attributed to C3 or to a lost serve from the log. | `sync.rs::try_reorg` |

Suspected, to be measured in S1: catch-up serving re-broadcasts every
block from the fork point as fresh envelopes (`serve_chain_from`), so
frequent forks inflate the relay's stored-event count and feed O4.

## 3. Decisions on the change request

The request (an operator's list A1-A5, B1-B5, D, E0-E5, F1-F4) is taken
with these changes:

- **B2 paging by `since=cursor` is rejected.** With newest-first replay
  it skips the older part silently (C7). Page backwards with `until`.
- **E1 (smallest id per height) is already built**: seal pacing
  (`SEAL_PACE_SECS`) plus `seal_held` as a `BTreeSet`. Forks come from
  nodes holding different signature sets, not from choosing differently.
  Replaced by S5.
- **E4 (cut a new checkpoint early) is rejected.** The blob summarizes
  state (accumulating applied entries, every consumed proposal id); a
  later cut is never smaller. S3 sends it in parts instead.
- **E3 and watchdog stage 4 (keep or adopt relays from other lists) are
  rejected.** They send traffic to a relay the group voted out, or read
  relays this node never confirmed, bypassing the clearnet/exposure gate.
- **D is cut to its core** (S4): retry with backoff, denser `known_heads`
  on retry, visible state. No sealing stop (it also stalls the winning
  branch), no relay widening.
- **E0 (new block-signature format with rounds) is deferred** to S6
  research. One signature per seat per (height, round) is not safe
  without locking: a block sealed at round r that others missed plus a
  different block at r+1 is a fork again. With honest seats this is a
  crash-fault consensus problem (Raft / Multi-Paxos class); evaluate an
  existing implementation before designing one.
- **E5 is covered by S5** (one sealer per height means one attestation
  list).

## 4. Stages

Each stage: red test first, then code; clippy 0; reviewed, green on
master.

### S1. Make divergence visible (A1-A4)

- `ChainBlockView` gains `hash` and `prev` (`verify.rs::block_hash`; empty
  for the pre-anchor entries). Built in `verify.rs::chain_block_view`.
- `status` gains a `chain_sync` object: `head_hash`, `catchup_from` +
  `catchup_since`, `fork_candidates` (count, min/max height),
  `last_fork` (peer, height, outcome `kept|rebased|relinked|dropped`,
  both hashes, time), `blob_bytes` + `blob_cap` (S3 warns from here),
  per-relay health with the last down reason
  (`connect_failed|session_ended|replay_bound`).
- Every fork decision logs `fork= mine= theirs=` (12 hex) and both
  proposal ids; the "nothing links" branch logs at INFO with
  `lowest= requested= suppressed=`.
- Catch-up traffic at INFO, rate-limited like the relay connect errors:
  request sent/received, served (`from=`, `blocks=`), not served
  (`reason=beyond_head|recently`).
- Tests: `hash` equal on two nodes with one chain, different for an
  attestation variant, `prev` equals the hash below; `last_fork` and
  `fork_candidates` set after a forced fork (`sync_tests.rs`).
- New `molt-core` fields: `#[serde(default)]`, MCP schema updated.

### S2. A full relay is read, not dropped (B2, C6, C7)

- Hitting the bound before EOSE ends the SESSION, never the supervisor
  (`Continue` with reason `replay_bound`); `Break` stays "consumer gone".
- During the stored replay the cursor is not advanced; it moves to the
  newest seen once EOSE arrives (fixes C7 for every relay).
- Paging: on the bound, re-place the REQ with `until` = oldest
  `created_at` seen in this replay (plus the overlap), keep the original
  `since`. A page whose oldest timestamp does not move back is "no
  progress": the relay goes `Down` with the long backoff, and the
  supervisor keeps running.
- Tests (`relay_runtime.rs` with `MockRelay`): 12 000 stored events with
  `with_history_bound(5000)` are all delivered and the relay stays Up; a
  relay that replays the same page forever is backed off, the
  subscription survives; a session cut mid-replay resumes without a gap.

### S3. The checkpoint blob travels in parts (B4, B5, C5)

- Measure first: S1's `blob_bytes`, plus a one-off breakdown by part
  (roster, per-surface applied, consumed ids) on the live node, kept out
  of the tree. Record the shape in this doc.
- New `WorkspaceEvent::CheckpointServedPart { blob_hash, index, total,
  bytes }`, split at `transport_plaintext_ceiling()` minus the envelope
  reserve. Pushed, not fetched: a rejoiner cannot ask (it has no
  workspace yet), which rules out the file-fetch path (`transfer.rs`).
  The receiver reassembles, checks `blob_hash`, then handles it exactly
  like `CheckpointServed`. Bounded reassembly buffer (one blob, total
  capped).
- `served_blob_fits` decides between one event and parts;
  `anchor_bootstrap` and `serve_chain_from` both use it.
  `an_oversized_checkpoint_blob_is_never_served` is replaced by its
  inverse.
- B5: a repeated recovery request from a seat whose restore is already
  sealed (same anchored transport key) re-sends the anchor bootstrap,
  debounced; no second admission.
- Old builds: an unknown variant must drop the frame, never stop the
  node or the outbox (test with the current deserializer).
- Tests: a blob of three frames bootstraps a rejoiner end to end
  (`nostr_recovery.rs`); a lost first bootstrap is recovered by the
  repeated request; byte-pin for the new event.

### S4. Catch-up that cannot stall (B1, B3, core of D)

- `catchup_from` carries its send time; after `CATCHUP_RETRY_SECS` (60,
  above `CHAIN_SERVE_DEBOUNCE_SECS` = 30) it may be re-sent. The
  delivery tick (`net/delivery.rs`, beside `drain_held_seals`) re-sends
  while `diverged` or `fork_candidates` is non-empty, exponential backoff
  to 10 min, reset on progress.
- A retry sends dense `known_heads`: the last 64 heights one by one, then
  geometric (`KNOWN_HEADS_MAX` grows accordingly).
- The fork winner serves its branch from the fork height once per peer
  and fork, with the `served_at` debounce.
- **Head beacon**: every `CHAIN_BEACON_SECS` (300, jittered) a node sends
  `ChainRequest { from_height: head + 1, known }` even when nothing looks
  wrong. Same wire event as today, so a v0.0.17 peer serves it; it is the
  only thing that finds a fork in a quiet republic, where no new block
  ever arrives to reveal it.
- After 15 min unhealed, once per episode (decided 2026-11-20): a wake
  reason, a chat line from this seat (`chain not healed: peer=<peer>
  since=<height>`), and `tracing::warn!(peer=, since=, secs=,
  "chain_heal_stuck")`; S1's status shows it too. Healing later logs
  `chain_healed` at INFO with the duration.
- Tests: three nodes on loopback, a fork, the loser's first
  `ChainRequest` dropped: all converge on one head hash without restart,
  and red with the retry disabled; the loser never asks: it re-bases
  anyway (B3); a converged republic sends no requests (no storm).

### S4a. Heal what v0.0.17 left behind (user requirement, 2026-11-20)

A republic that diverged under v0.0.17 heals by itself after the update,
without a restart by hand and without new proposals.

- **Forked running seats.** Open sends a catch-up request today
  (`session.rs`); S4's dense `known_heads`, the beacon and the winner's
  push do the rest. Mixed versions heal as well: a v0.0.17 peer serves
  requests (`serve_from_for`) and runs the deep tie-break on a pushed
  branch, so one updated seat suffices between itself and each peer.
  Depth is no limit in practice (`CATCHUP_BUFFER_WINDOW` = 4096).
- **A restored seat stuck without its anchor (O3).** It cannot ask, and
  its repeated recovery request is dropped. Every survivor re-pushes the
  anchor bootstrap (in parts, S3) while a seat restored by a sealed
  `Restored` block has not been heard on the chain since (no
  `ChainRequest`, no block from it), every 10 min, at most 12 times per
  restore. The rejoiner needs the new version to read the parts.
- **Displaced decisions.** A re-base returns the losing branch's
  decisions to the vote, and each seat re-signs its own approvals
  automatically (`own_approvals` is persisted in the dump). No operator
  action.
- Tests: two seats with a persisted fork 30 blocks deep, reopened with no
  traffic: one head hash within a fixed bound; the same with one seat on
  a build without S4 (stands in for v0.0.17); a quiet republic forks
  while running and the beacon heals it; a restored seat that missed its
  bootstrap materializes from a re-push.

### S5. One sticky sealer (replaces E1/E5)

Revised 2026-11-20 after the S6 research: a rotating sealer (`h mod n`)
makes every height a hand-over, so a disagreement between presence views
can fork at any height. A sticky leader confines that to leader changes.

- `leader` = the lowest-named roster seat that is not silent
  (`chain_lag().silent`). It keeps the role while it delivers; it loses
  it only by going silent or by missing a grace. Only the leader seals.
  Everyone keeps signing and gossiping approvals as today.
- One deterministic choice for the leader and every fallback: the
  smallest proposal id holding m valid signatures at head+1. A late
  original against a fallback then usually carries the same change, so
  the "fork" is an attestation variant, which R1 re-links without a
  re-vote.
- The leader seals without the seal pacing round (the pacing exists so
  that many sealers' signature sets converge); everyone else keeps it.
- Fallback: if no block at h is seen `SEALER_GRACE_SECS` (60) after this
  node could have sealed, the next seat in name order that is not silent
  becomes leader for this node. Before its first seal it sends a
  `ChainRequest` with its `known_heads` and waits one pacing round, so a
  block the old leader sealed has a chance to arrive first.
- Why 60 and not 15: the leader first has to receive the approvals over
  Tor (1-5 s per frame) and publish (1-5 s), so 15 s is the normal tail,
  not a failure, and a takeover there would re-create the forks S5
  removes. A late takeover only delays; an early one forks. S1's logs give
  the measured approval-to-block latency; the grace is set to its p99
  times two once measured, 60 until then.
- No format change: `verify_chain` and every signed layout stay as they
  are; old builds mixed in keep sealing freely and only fork more.
- A reducer, not a guarantee: every seat still signs every open
  candidate (C1), so a second sealer can always collect m signatures.
  Forks become impossible only with S6.
- A cut (n-of-n) and a consented restore go through the same rule.
- Tests: the E2E in `tests/chain_convergence.rs` extended to three seats
  with bursts; fork count (S1 counter) recorded with and without S5, and
  S5 well below; a silent leader is replaced after one grace, never
  stalls the chain.

### S6. Fork-free sealing: a Flexible-Paxos lock (research done 2026-11-20)

Not in this release. Only if S1's counter still shows forks that matter
after S5 (the field rate before S5 was 79 of ~124 heights).

Verdict per candidate (crash-fault model, honest seats):

| candidate | verdict |
|---|---|
| openraft, tikv raft-rs | leave the crates: they bring their own log, membership and RPC (duplicating chain, roster and sync), carry no member signatures, and use a majority where governance is m-of-n. Take the ideas: term, persisted vote, sticky leader. |
| Multi-Paxos / Flexible Paxos | take as the design: chain heights are the slots, the m-signature block is phase 2; Flexible Paxos needs only Q1 + Q2 > n, so Q2 = m gives Q1 = n - m + 1. No maintained Rust crate found. |
| Viewstamped Replication | same class as Raft, nothing extra. |
| Tendermint / malachite, HotStuff, Narwhal/Bullshark | leave: BFT quorums need more than 2/3 of the seats, so a 2-of-3 republic halts on one offline seat; malachite is alpha, Narwhal archived. |
| CRDT / DAG | leave: signatures cover the height, so a merge is always a re-vote; order matters for wiki, membership and cuts. R1's re-link of attestation variants stays the fork-tolerant part. |

Design sketch (honest seats; no signed-layout change):

- Approval split from position signing: an approval becomes an unsigned
  intent; the leader announces "height h, ballot b, change P" for a
  proposal with m intents; seats sign `(rid, h, P)` only for the current
  leader's announced P and only if b is at least their promise. Costs one
  more round trip.
- Takeover is phase 1: the new leader collects n - m + 1 promises; each
  carries the seat's accepted (ballot, change) at h, and the highest one
  must be re-proposed. Quorum overlap guarantees a sealed block is seen.
- Persisted per seat, written before sending: the promised ballot and the
  accepted (ballot, change) per open height. Forgetting it on restart
  breaks safety.
- The ballot travels in MLS-authenticated gossip as additive
  `#[serde(default)]` fields; signing it or `prev` (R6) is only needed
  against Byzantine seats (a versioned layout).
- Activation: only once every roster seat advertises support; one seat
  that signs everything breaks the quorum overlap.

### S7. Every error toast lands in the log (same release)

User requirement (2026-11-20): an error the GUI shows as a toast can be
read again in the regular error log (Settings › Error log, `read_log`).

- One choke point: `app.slint`'s `show-toast-error` calls a new callback
  `toast-error-shown(msg)`; `molt-ui` handles it with
  `tracing::warn!(toast = %msg, "gui_error")`. That covers the 52 Rust
  call sites (`invoke_show_toast_error`) and the two in `.slint` alike,
  and a new caller cannot forget it.
- WARN, because the log tab's narrow levels filter to `LogFilter::Warn`
  and many toasts are refusals, not faults.
- The leading warning sign of `error_toast` is stripped in the log line.
- Test (live-preview GUI test): showing an error toast writes one
  `gui_error` line with the text; a plain toast writes none.

## 5. Compatibility

S1, S2, S4, S5 change only local behaviour or local output; old and new
nodes run mixed. S3 adds one event variant (old nodes must drop it). No
signed layout changes anywhere in this plan.

## 6. Acceptance

- `tests/chain_convergence.rs`: three seats, many concurrent proposals,
  ~10 % frames dropped, one relay dead, one relay holding more than the
  bound: all seats end on one head hash within a fixed bound, no restart.
- The same with S5: fork count recorded, well below the S4-only value.
- A rejoiner restores against an over-cap blob.
- A divergence persisted by a v0.0.17-shaped chain heals after the
  update without restart by hand (S4a).
- fmt, clippy 0 per crate, all tests green.

## 7. Decisions (user, 2026-11-20)

1. S5 grace: 60 s plus skipping silent seats, re-set from S1's measured
   latency; a sticky leader instead of `h mod n` (S6 research).
2. 15 min unhealed: wake, chat line and a log warning (S4).
3. Order: S1 -> S2 -> S3 -> S4 + S4a -> S5, with S7, released as one
   version.
