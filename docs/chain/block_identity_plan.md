# Block identity without the signer set

**Status: PLAN, not started; candidate for v0.1.3 beside
`chain_rescue_plan.md`.** Source: public issue #10, part 1 (a 2-of-3
republic; counts and shapes only). Part 2 of the issue (the stale accept
window) is fixed in v0.1.2, `docs_archive/transport/delivery_guarantee.md`
§11; §9 records what v0.1.2 did not take from it. Code cited on master.

## 1. The diagnosis

**Verdict: right in mechanism, overstated in scope.** The link hash does
commit to the signer set, so one decision sealed by two different
m-subsets yields two block hashes and a "fork". But the `tip_swapped`
outcomes the issue counts cannot be pure variants: at the tip, a
contender with the same change and the same `prev` returns early
(`chain/sync.rs:1020-1025`); `tip_swapped` (`:1051`) needs a different
change. The `kept` outcomes can be pure variants: `try_reorg` logs `kept`
(`:722-724`) BEFORE it checks `same_decisions` (`:734`).

| what | content | file |
|---|---|---|
| link bytes | `molt-chain-block-v1\0` ‖ height ‖ `prev` ‖ `approval_bytes(republic_id, height, change)` ‖ every `(member, sig)` sorted by member | `molt-core/src/chain.rs:822-838` |
| block hash | `content_hash(block_link_bytes)` | `molt-engine/src/chain/verify.rs:97-99` |
| member signature | `approval_bytes(republic_id, height, change)`: no `prev`, no signer set | `chain.rs:215-340`, `verify.rs` `block_signers`/`valid_signers` |

Where the hash is used, and what a variant does there today:

| use | effect of a variant |
|---|---|
| `prev` links | every block above a variant differs too, so the whole suffix reads as another branch |
| `tie_break` at the tip | same change + same `prev`: ignored, or stashed and relinked when lower (`sync.rs:1020`) |
| `tie_break` below the tip / foreign `prev` | `note_divergence` (`chain_diverged` mark, WARN `a block from another branch`, notice `chain-diverged:`), stash, `try_reorg` (`:1027-1036`) |
| `try_reorg` | the lower hash at the first difference wins. The higher side logs `fork_decision outcome="kept"`, serves its branch (`serve_branch_to`) and re-sends standing votes (`:722-729`). The lower side relinks (`relink_onto_variant`, `:805`), logs `relinked`, re-applies the fresh blocks |
| while marked | `chain_unsettled` holds: cuts and relay changes wait (`sign_blocker`), restores refused `chain diverged` (`chain/membership.rs` `restore_gate`), the heal tick retries, `heal_stuck` after rounds |
| stance work (v0.1.2) | every head move re-signs standing approvals; a heal re-sends them (`governance.rs` `resend_standing_approvals`, `persistent_chain.md` §7.1) |
| catch-up | `ChainRequest.known` samples are (height, hash) (`sync.rs` `known_heads`); `serve_from_for` matches by hash, so a variant below serves a longer suffix than needed |
| `seats[].head_hash` (v0.1.2) | two seats with the same decisions report DIFFERENT heads; the seat table prints `other branch at h` (`molt-ui/src/surfaces.rs:313-322`) |
| checkpoint state | `CheckpointState` (`chain.rs:517-585`) holds roster, applied entries, consumed ids, anchors, relays, `upto`: no block hash, no signatures. `applied_sigs` (`projection.rs:371`) is a display map, never hashed |
| checkpoint block | an ordinary block: its `prev` is the hash of block `upto`. Cuts seal n-of-n (`governance.rs:739-743`), so an honest cut has one signer set |
| republic id | content-derived from the roster (`molt-republic-id-v2`): no block hash |
| genesis | n-of-n, Ed25519 is deterministic: one signer set, no variant |
| recovery anchor | `RecoverAnchor` ships the anchor EVENT (`molt-net/src/invite.rs:657`); the rejoiner hashes it itself. The consent binds a transport anchor, not a block hash |
| Strict Order Mode (dark) | `lineage` is a list of change digests (`chain/lock.rs:20-41`): already decision-only |

So 11 variant "forks" look like real forks to an operator: WARN lines, a
notice, a `chain_diverged` mark, held cuts and restores, re-sent votes,
`other branch` in the seat table, though nothing was decided twice.

## 2. Where variants come from

| path | why two subsets | code |
|---|---|---|
| two seats both first in the sealing line | the line is each seat's OWN view (`unseen_seats`, `diverged`), so A and C can both rank 0 and seal with what each holds | `governance.rs:786-803` `sealing_line`, `turn_holder` |
| fallback sealer | after `rank × grace` (+ ask) a later seat seals with its own set; the first seal may only be late | `governance.rs:71-75` `sealer_wait`, `:810-850` `sealer_turn` |
| "m lowest-named valid signers" | deterministic over the signatures THIS seat holds, not over all n | `governance.rs:6`, `:656` |
| restore | m-1 survivors + consent; two coordinators hold different survivors | `governance.rs` seal `need`/`truncate` (`:739-750`) |
| cut | n-of-n: one set | |
| genesis | n-of-n: one set | |

Fixing it at the source is not possible in general. "Always the m
lowest-named among all n" fails when one of them never signs. "Seal only
with the canonical set once complete, else wait" turns a silent seat
into a stall. So source-side canonicalisation cannot be complete; the
identity is the root fix.

## 3. The proposal

Block identity v2:
`content_hash("molt-chain-block-v2\0" ‖ height ‖ prev ‖ approval_bytes(republic_id, height, change))`.

The signatures become an attestation attached to the block. Any valid
m-subset (or m-1 + consent for a restore) is acceptable and
interchangeable.

| point | definition |
|---|---|
| member signature | unchanged: it never covered `prev` or the signer set (CLAUDE.md, "Signatures are position-bound"). NOT a signature format change, only a link layout change, so the tag bumps and `verify_chain` updates together |
| `verify_chain` | unchanged except `block_hash`: the identity is v1 below the activation height H, v2 from H |
| the boundary | the block at H links to the v1 hash of H-1; every later block to the v2 identity of its predecessor. `prev(b) = id(b.height - 1)`, with `id = v1` if height < H else v2 |
| two seats, same identity, different attestations | NOT forked. The duplicate check becomes identity equality (today `existing == &block`, full struct, `sync.rs` `tie_break`). Neither upgrades its attestation; both verify |
| the fork rule | unchanged: the first-difference comparison now differs only where the DECISIONS differ |
| `relink_onto_variant`, `same_decisions` | unreachable above H; kept for pre-activation history |
| `seats[].head_hash`, `known` samples | comparable across seats; the seat table's `other branch` means one |
| stance machinery | runs on real head moves only |
| branch choice | `BranchChosen.tip_hash` (`chain_rescue_plan.md` §1) names a decision, not an attestation. Under v1 the choosers would have to agree on one variant |
| checkpoint | layout unchanged (`checkpoint-v10`); a cut at or above H is v2-linked. [INFERRED] The effective feature set in the blob says whether everything from its anchor up is v2, since H ≤ the cut's height when the enabling block is at or below `upto`. To verify while building S1 |

## 3a. Every identity comparison (issue #10 comment, caveat 2)

All of these must use the height-versioned identity (`block_hash` made
version-aware), never struct equality and never `block_link_bytes`
directly. Lines are as of master (`git grep block_hash\(|block_link_bytes\(|== &block`).

| site | today | after |
|---|---|---|
| `verify.rs:97` `block_hash` | v1 link hash | v1 below H, v2 from H: the single source |
| `verify.rs:260, 446, 504, 631, 1319` | genesis head, `prev` check, step head, `describes`, suffix anchor head | via `block_hash` |
| `governance.rs:771`, `lock_wire.rs:400, 1544` (dark) | a sealed block's `prev` = head hash | unchanged (head hash is the identity) |
| `sync.rs:169` | `block.prev != head.hash` | unchanged |
| `sync.rs:986` `existing == &block` | struct equality (signatures included) | IDENTITY equality; a different attestation is a duplicate (clears the divergence like today) |
| `sync.rs:998-999` | `content_hash(block_link_bytes(..))` directly | `block_hash` |
| `sync.rs:1010`, `:674`, `:710` | `shared_prev`, candidate links, suffix contiguity | via `block_hash` |
| `sync.rs:720`, `:686-688` | first-difference comparison, `fork=unlinked` hashes | via `block_hash` |
| `sync.rs:734-736` `same_decisions`, `:1019` | change equality | unchanged; unreachable for v2 variants |
| `sync.rs:817` | relink rewrites `prev` | pre-H only |
| `sync.rs:293, 317, 326, 641` | `known_heads`, `dense_known_heads`, `holds_block`, `serve_from_for` | via `block_hash`; `ChainRequest.known` samples become comparable |
| `sync.rs:182, 873-874, 989, 1247` | log hashes, served range, `note_refused` | via `block_hash` |
| `sync.rs:1334, 1341` | `note_on_branch`, heal-round key | via `block_hash` |
| `net/seat_report.rs:71, 78` | `seats[].head_hash` | the identity |
| `projection.rs:666` | `read_chain` row hash | the identity |
| `recovery.rs:1594, 1641` | test chains built with v1 links | test fixtures: build by version |
| `examples/chain_forensics.rs` | offline hashes | by version |

No cache or dedup is keyed by a block hash: `pending_blocks` and
`fork_candidates` are keyed by height, the accept window by seq, the
seen ring by ciphertext.

**Two attestations for one identity.** Keep the first one verified
here and never replace it. The block identity, checkpoints and the
state do not depend on it. What differs per seat is the signer list
`read_chain` shows and the bytes of an export, as today across variants.
Replacing it by "the lowest-named set" would buy byte-identical exports
at the price of a rewrite path in a verified chain; not worth it.

## 3b. `prev` in the signed bytes, same activation (decided 2026-11-24)

Issue #11; argued in `chain_rescue_plan.md` §11b. With the v2 identity
the member signature covers `republic_id ‖ height ‖ prev identity ‖
change` under `molt-chain-change-v3`. One activation carries both
layouts: the feature key of §4 switches link and signature at the same
height.

| | |
|---|---|
| buys | a signature attests the chain below it and is valid on one branch only; two branches with m signatures each then prove a seat signed on both; the precondition for one vote per slot |
| does not buy | two changes sealed on the SAME predecessor stay possible |
| genesis | unchanged: its signatures are the roster attestations, it has no `prev` |
| restores, cuts | sign `prev` like any block; consent rules unchanged |
| dead after activation | `relink_onto_variant` (it relies on an unsigned `prev`) |

Gate before it is built, a measured run and not an argument: a 2-of-3
republic with one seat lagging by one block, 50 proposals. Compare the
time from propose to sealed with and without the binding. Pass = no
wait beyond the fork rule settling; a seat behind already refuses to
vote (`refused_lagging`), so none is expected. If it fails, v2 ships
alone and Q8 reopens.

Tests on top of §6: a signature made on one predecessor is rejected on
another; byte pins for `molt-chain-change-v3`; standing votes re-sign
after a tip change at the same height; an older build stops at the
first v3 block.

## 4. Activation

Every seat must switch at the same height, or they compute different
`prev` values and reject each other's blocks for good (rule `link`).

| option | how | old builds | verdict |
|---|---|---|---|
| (a) feature key by vote | `set_features` naming `block-v2`; active from the height after the enabling block. Propose and approve refused unless every seat announced `molt-block-identity-v2` in its mirror declaration `caps` (`net/mirror.rs:722`, `prefs.seat_caps`) | an older build refuses the KEY at propose and in a charter (`canonical_features`, `verify_canonical_features`, `molt-core/src/lib.rs:173-182, 272-300`). It does not check a received `set_features` block, so it applies the block and then refuses the first v2 block (`link`). It stops extending, and its frames show it behind | recommended for existing republics |
| (b) new foundings ratify `block-v2` in the charter | v2 from height 1; the genesis keeps its v1 hash (no variants, republic id untouched) | an older joiner refuses the unknown key at sign-what-you-see | recommended for new foundings, together with (a) |
| (c) a flag day by height in config | | a mismatch is a permanent fork | rejected |

Downgrade wall: after H an older build cannot follow. The precedent is
`STORAGE_VERSION_LOCK = 4` (`molt-core/src/lib.rs:1769`). A manifest bump
(`STORAGE_VERSION_BLOCK_V2 = 5`), raised when the enabling block applies,
makes an older binary refuse the workspace at the manifest gate instead of
reading a chain it cannot verify.

## 5. What it does not fix

- Two DIFFERENT decisions sealed at one height stay real forks. The fork rule decides them, and `chain_rescue_plan.md` handles the ones that do not heal.
- Share of variants in the field: 11 of 11 in the issue's window by its author's reading. §1 shows at least the `tip_swapped` ones differed in their change.
- An earlier field day had fork decisions at 79 heights in about 124 blocks, with an unknown split.
- The v0.1.2 `fork_decision` line logs both hashes and `mine_id`/`theirs_id`, so operators can count variants (equal ids) against real forks.

## 6. Risks and tests

| test | pins |
|---|---|
| byte pin of the v2 layout; v1 pin unchanged | the tag bump |
| mixed chain v1 then v2, with a variant just below H | the boundary definition; a variant below H is still relinked |
| 2-of-3, every block sealed by different subsets on different seats | zero fork decisions, identical head identity on all seats |
| pre-activation history with relinked variants | still verifies |
| a cut below, at and above H; re-anchor across the boundary | suffix walks agree |
| recovery of a seat after activation | the rejoiner hashes the anchor and the suffix as v2 |
| an older build meets the enabling block | applies it, refuses the next block (`link`), stops |
| `chain_convergence.rs` keystone before/after | fork decisions and frames per block (expect fewer re-served votes) |

Risk: the identity no longer commits to who attested. A block with the
right identity and an invalid attestation must still be refused
(`verify_chain` checks m valid signers, unchanged), and a seat must never
replace a valid attestation by an invalid one of the same identity.

## 7. Staging

| step | content | agent-h |
|---|---|---|
| S1 | v2 layout, `block_hash` by height, verifier + duplicate check, byte pins, boundary tests | 10 |
| S2 | feature key, capability, propose/approve gate, manifest bump, charter path for new foundings, status (`chain_sync.block_identity`) | 10 |
| S3 | the convergence keystone, dead-path cleanup above H, docs (`persistent_chain.md` §5) | 6 |

Order with v0.1.3: majority rule (rescue S1), then this, then the branch
choice (rescue S2-S3), so `tip_hash` names decisions from the start. One
activation vote can carry `block-v2`; the branch choice needs no vote
(additive variant). Capabilities: one per mechanism; both ship in one
build.

## 8. Open questions (user)

| # | question | recommendation |
|---|---|---|
| Q1 | activate existing republics by a `set_features` vote? | yes, gated on every seat's capability |
| Q2 | new foundings v2 from the genesis? | yes, in the charter |
| Q3 | ship in v0.1.3 before the branch choice? | yes |
| Q4 | one capability for both v0.1.3 chain features, or one each? | one each: they activate differently |
| Q5 | a feature key outside the surface vocabulary (like `chain-lock`)? | yes, the same exception as `chain-lock` (`molt-core/src/lib.rs:175`) |

## 9. Issue #10 part 2: suggestions v0.1.2 did not take

- **(i) A per-seat incarnation number in the envelope, bumped by each `Restored` block.** v0.1.2 instead makes the re-key merge the single reset and persists it before the ratchet (`delivery_guarantee.md` §11). An incarnation number needs a wire field older builds ignore, and does not help a seat that missed the merge under an older build.
- **(ii) A `stale_seq floor=X` reply, or starting a recovered device's seqs above anything seen.** The receiver keeps a gap-free frontier (`net/delivery.rs:90-100`, `SenderHold`). A sender jumping far above it is held above `frontier + ACCEPT_WINDOW_BITS` (`:143`) and the skipped seqs become missing gaps (`note_missing`). The receiver then asks for seqs the sender never sent, until they settle as unfillable.
- **(iii) Surfacing:** done in v0.1.2: the `dup_streak` hint, `status.seats[]`, the seat table and `resync_seat`.
