# The Persistent-Change Chain

How a MoltRepublic republic keeps **one** shared, tamper-evident record of
everything that changes its persistent state. This document describes the chain
**abstractly** — what a block is, what it is signed over, and what a verifier
checks — then points at the code.

It is the state-model companion to `founding_ritual.md`: the founding produces
the chain's first block, and everything the republic later ratifies extends it.

> **Status (2026-07-08).** Phases 1–2 are implemented. Phase 1: the block
> format, its canonical bytes, and full hard-reject verification. Phase 2: the
> chain is persisted + verify-loaded, and **real threshold governance runs over
> the mesh** — a chain-governed republic signs approvals, gossips them, seals a
> block at m distinct signatures, broadcasts it, and every member converges
> (proven end-to-end over the direct MLS mesh in
> `two_instances.rs::founding_governs_over_the_direct_mesh`). Phase 3 (catch-up
> sync) is wired too — out-of-order buffering + a survivor-serves-the-suffix
> request/response. Phase 4 (recovery) is REAL end to end — the recovery ritual
> (`recovery_ritual.md`) re-admits a total-loss member by threshold block,
> re-keys the MLS group, serves the chain over the recovery channel, and the
> rejoiner verifies from genesis + materializes. §10 tracks the split.

---

## 1. Principle: one branch, threshold-signed, self-authenticating

Everything that **persistently** changes a republic is a *commit block* in a
single, strictly linear chain — "git patches", each referencing the last. Three
properties define it:

- **Threshold-signed.** A block is one change that reached the republic's
  *m*-of-*n* approval, carrying those *m* member signatures with it. The founding
  is block 0, sealed *n*-of-*n* (unanimous) — it already *is* such a block.
- **Self-authenticating.** A member — or a rejoiner who fetched the chain from an
  **untrusted** peer — verifies it alone: the signatures and the links, no live
  mesh and no trust in the deliverer. One surviving member with the full chain is
  enough for everyone else to catch up to its latest state (§8).
- **Single branch.** The settled chain is always linear — no persistent forks. A
  race between two approved patches for the next slot is serialized: one becomes
  step *n+1*, the loser re-bases onto *n+2* (§7). Members may briefly hold
  different heads, but they converge to an identical chain.

## 2. What is a block — and what is not

Chained (persistent, converged): the **founding**, every **gated** surface
transition that reaches threshold (`Applied`), and **membership** changes
(a seat joins, or a member re-keys after recovery).

**Not** chained (ephemeral, *flüchtig*): **chat** and its reactions/edits, and
the **deliberation itself** — proposing and approving is off-chain gossip. Only
the *committed* change becomes a block, with its *m* signatures bundled. Losing
ephemeral traffic is acceptable; a rejoiner is not handed old chat.

The boundary is not "chat vs. not-chat" but **ephemeral by default; the moment
content becomes durable republic knowledge it crosses into the chain through a
gated, threshold-approved change.** Deliberately persisting a chat excerpt into
the shared brain is itself such a change — so *that* promotion is a block, while
the live conversation stays ephemeral.

## 3. Anatomy of a block

```
ChainBlock {
  height : u64             // 0 = genesis, strictly monotonic, no gaps
  prev   : hash            // hash of the previous block's link bytes (§5);
                           // GENESIS_PREV (32 zero bytes) for block 0
  change : ChainChange     // Genesis | Applied{proposal_id,surface,payload}
                           //         | Membership{op,member,identity_pk}
  sigs   : [ (member, signature) ]   // n-of-n at genesis, m-of-n afterwards
}
```

`ChainChange` is additive-only, exactly like `WorkspaceEvent`: a new kind of
gated mutation appends a variant, and a reader that meets an unknown variant must
refuse to extend the chain rather than apply a partial history.

## 4. The genesis is block 0

The founding ritual already produces `roster_canonical_bytes` signed by all *n*
members (the attestations — see `founding_ritual.md` §4–8). The chain **roots on
exactly those bytes**: block 0's `change` is the sealed constitution and its
`sigs` *are* the founding attestations. So the founding needs no new signing
path, and a rejoiner verifies the constitution the same way every member
ratified it. `roster_canonical_bytes` is untouched (still `molt-roster-v2`).

## 5. What is signed, and what links

Two distinct byte strings, deliberately separated:

- **Approval bytes — what the *m* members sign.** *Position-bound*: the block's
  `height` is folded in.

  ```
  approval_bytes(republic_id, height, change) =
      genesis  → roster_canonical_bytes(republic_id, m, n, identities, agenda)
      other    → "molt-chain-change-v1\0" ‖ republic_id ‖ height ‖ change-fields
  ```

  Because `height` is inside the signed bytes, a signature authenticates the
  change **at that exact sequence number**. A block cannot be reordered, moved,
  or spliced onto a different prefix without the members re-signing — which is
  precisely what a re-base is (§7). Reorder/splice is therefore dead: heights are
  gapless, every height needs its own *m* valid signatures, and the genesis is
  content-fixed.

- **Link bytes — what `prev` points at.** The next block's `prev` is the SHA-256
  of this block's link bytes, which commit to the height, the previous `prev`,
  the approval bytes, **and the exact signature set** (sorted by member):

  ```
  block_link_bytes = "molt-chain-block-v1\0" ‖ height ‖ prev
                     ‖ approval_bytes ‖ (member, sig) sorted by member
  ```

  So neither the change nor its signatures can be altered without breaking every
  downstream `prev`. `prev` is a **structural** link the verifier checks; it is
  not itself covered by the member signatures (the height already pins position).

## 6. Verification — every check is hard-reject

`verify_chain` walks from the genesis and returns the verified head (height,
hash, the content-derived republic id, *m*, and the live roster). Any failure
rejects the **whole** chain — a partially-valid chain is not a thing, because a
rejoiner that trusted a prefix could fork the republic's state. The checks:

- **Genesis (block 0).** height 0, `prev = GENESIS_PREV`; `republic_id` is the
  neutral content-derived value recomputed from the roster; `0 < m ≤ n`; the
  roster has *n* identities; and it is signed **unanimously** — every anchored
  member's attestation verifies over `roster_canonical_bytes`.
- **Each later block.** `height = prev.height + 1` (gapless); `prev` equals the
  predecessor's link hash; at least *m* **distinct** roster members have a valid
  signature over the block's approval bytes (a repeated or unknown signer never
  inflates the count); and no proposal id is `Applied` twice.
- **Roster evolution.** A `Membership` block grows (`Joined`) or re-keys
  (`Restored`) the roster, so the newcomer's or rekeyed member's signatures count
  on the blocks that follow.

## 7. Ordering — single branch, re-base = new sequence number + re-sign

There is exactly one branch. A patch that gathers its *m* approvals is
*einsortiert* — placed as the next step at the current head. If two patches race
for slot *n+1*, one wins the slot and the other **re-bases**: it takes a **new
sequence number** (*n+2*), and because the approval signatures are height-bound,
its members must **re-sign** over the new height. There is never a lasting fork
and never a choice between rival histories — only a transient difference in how
far each member has advanced along the same line.

## 8. Threshold and recovery fall out of the chain

- **Real threshold, no FROST.** A gated change is authorized iff its block
  carries *m* valid, distinct member signatures. The threshold *is* the chain; no
  separate threshold-signature machine is needed. (This replaces the current
  "counted approvals" simulation — see `events.rs`.)
- **Recovery = catch-up.** A returning member fetches the linear chain from any
  peer, verifies it here from the genesis, and is current — no live
  reintegration. Re-keying its seat is a `Membership{Restored}` block, itself
  threshold-approved, so the recovery is recorded in the same tamper-evident
  line. (Confidentiality/transport is MLS's job; the chain owns authenticity.)
- **Checkpointing (later).** A long chain will eventually be compacted —
  squashing settled history into a signed checkpoint so newcomers need not replay
  everything. Out of scope until the chain is wired; noted so the format leaves
  room.

## 9. What the chain guarantees a verifier

Having verified a chain, a member knows — not trusts — that:

1. **The constitution is genuine.** Block 0's id matches its roster content and
   carries every founder's signature (the founding guarantees, `founding_ritual.md` §8).
2. **Every change was approved.** Each block has *m* distinct member signatures
   over its exact change at its exact position.
3. **Nothing was reordered, inserted, or dropped.** Heights are gapless and the
   `prev` links form an unbroken line from the genesis; moving anything breaks a
   signature or a link.
4. **No change was applied twice.** Proposal ids are unique across the chain.

## 10. Real vs. planned

- **Real (Phase 1).** `ChainBlock`/`ChainChange` and their canonical bytes
  (`molt-core`), the block hash (`molt-storage`), and `verify_chain` with its
  hard-reject checks (`molt-engine`), unit-tested for build/verify and for
  tamper rejection (bad sig, broken link, height gap, below-threshold, repeated
  signer, double-apply, forged genesis id, roster growth).
- **Real (Phase 2).** The founding roots + persists block 0 (`chain.state`,
  sealed like `transport.state`), and a reopen verify-loads it and restores the
  signing key. `cmd_approve` on a chain-governed republic signs the change at
  the head's next height and gossips the signature over the mesh
  (`crosses_wire`); the engine collects distinct valid signatures and, at *m*,
  deterministically seals a block (the m lowest-named signers, so concurrent
  committers seal the byte-identical block), broadcasts it, and re-projects the
  gated surfaces from the chain. A peer verifies + adopts the broadcast block.
  The legacy counted simulation is disabled for chain republics. Proven E2E over
  the direct MLS mesh; a `receive_block` unit test covers the follower path.
  Single-branch re-base (new height → re-sign) and a tip tie-break by min-hash
  are wired; deep reorg + concurrent-race stress are light until Phase 3.
- **Real (Phase 3).** Catch-up sync: a block that arrives ahead of the head is
  buffered (`pending_blocks`) and applied once the gap fills, so a suffix
  converges regardless of arrival order. A lagging or reconnecting member
  broadcasts `ChainRequest{from_height}` (also on reopen); any peer that is
  further ahead re-serves those blocks straight from its own `chain.state` (as
  `Committed`), so **a single survivor with the full chain reconstitutes it for
  everyone**, independent of who originally committed each block. Trade-off: the
  server re-serves through the log (the outbox), so serving grows its log — the
  chain compaction the model already anticipates addresses this.
- **Real (Phase 4).** Recovery as catch-up-from-genesis + MLS re-key: a
  `Membership{Restored}` block re-admits the seat by threshold, the coordinator
  re-keys the group (`restore_member`) and broadcasts the raw commit over the
  mesh, the Welcome carries the whole chain over the recovery channel, and the
  rejoiner hard-verifies from block 0 and materializes (`recovery_ritual.md`
  has the ritual + its own implementation map; proven E2E in
  `two_instances.rs`). Still open: concurrent-race hardening (deep reorg beyond
  a tip tie-break), moving deliberation truly out-of-band (today it rides the
  local log as transport), and the rejoiner's mesh re-establishment (dynamic
  mesh membership).

## 11. Implementation map

- **Types + canonical bytes** — `crates/molt-core/src/chain.rs`
  (`ChainBlock`, `ChainChange`, `MembershipOp`, `approval_bytes`,
  `block_link_bytes`, `GENESIS_PREV`).
- **Block hash** — `crates/molt-storage/src/lib.rs` (`content_hash`; alongside
  `republic_id`, `identity_sign`/`identity_verify`).
- **Verification** — `crates/molt-engine/src/chain.rs` (`verify_chain`,
  `block_hash`, `ChainHead`; a strict generalization of
  `founding.rs::verify_sealed_roster`).

The founding that produces block 0 is `founding_ritual.md`; the transport that
will carry blocks and gossip is `concept-transport-simplex-tor.md`.
