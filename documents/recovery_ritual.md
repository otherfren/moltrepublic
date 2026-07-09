# The Recovery Ritual

How a member who lost **everything but its recovery phrase** returns to a
republic: re-authenticated, re-admitted to the encrypted group, and caught up to
the latest shared state. This document describes the ritual **abstractly** — the
actors, the messages, the secrets that bind them, and what each side verifies —
then points at the code.

It is the total-loss twin of `founding_ritual.md` (a member *joins* a new
republic) and the recovery half of `persistent_chain.md` (a member *catches up*
the chain). Read both first.

> **Status (2026-07-08).** Spec. The primitives are built and unit-tested
> (`make_seat_proof`/`verify_seat_proof`, `restore_member`, the
> `ChainChange::Membership{Restored}` verifier, catch-up + headless
> genesis-adoption); the ritual that assembles them is the work this spec
> drives, test-first.

---

## 1. Principle: same identity, fresh device

A recovering member `R` holds **only its recovery phrase**. From it — and nothing
persisted — it re-derives the *same* identity keypair it always had (derivation
is deterministic: `founding_ritual.md` §2). So its roster identity is unchanged;
what it lost is *device state*: the MLS group ratchets, the transport queues, and
its local chain.

Three properties follow and shape the whole ritual:

- **The phrase is the credential.** Only the phrase re-derives `R`'s identity
  signing key, so a challenge signed by that key **is** proof that this is the
  real seat owner. A leaked recovery link cannot answer it.
- **Re-admission is a group decision.** No single survivor lets `R` back in;
  the group re-admits it by a **threshold-approved chain block** (m-of-n), the
  same gate every persistent change passes.
- **Recovery is self-authenticating.** `R` trusts nothing a survivor *says* —
  it verifies the chain from the genesis (signatures + links), that its own
  `(name, key)` is in the roster, and that the republic id is content-derived.
  One survivor with the full chain suffices (`persistent_chain.md` §8).

---

## 2. Actors and their secrets

- `R` — the rejoiner. Re-derives its **identity keypair** (Ed25519) and a fresh
  **MLS `KeyPackage`** `kpᵣ` from that same key (one identity, two anchors). It
  has no chain, no MLS state, no queues.
- `S` — a surviving member that helps `R` back in (the *recovery coordinator*).
  It holds the group's MLS state (with `R`'s stale leaf still in it) and a chain
  at height `H`. Any survivor can be `S`.
- The other survivors — they approve the re-admission and hold the chain.

The only thing shared a priori is `R`'s phrase (which `R` alone holds) and the
recovery link `S` mints (§3).

---

## 3. The recovery link

`R` cannot reach the group from its phrase alone: the phrase derives identity and
a workspace id, but **not** the group's transport queues (those lived in the lost
`transport.state`). So `S` mints a **single-use recovery link** and shares it
off-band, exactly like a founding invite for an already-filled seat:

- a **transport handover** — where and how to reach `S` (a recovery queue `S`
  receives on, its wrapping key); and
- a **recovery ticket** — a high-entropy, single-use secret.

The security core is the same as founding: the ticket binds the request by a
seat proof (§4), so a bare leaked queue address cannot start a recovery and a
replayed ticket is dead. A recovery link differs from a founding invite only in
intent (an existing seat, manually granted) and that approval is **not**
automatic — the group votes (§4, step ❹).

---

## 4. The ritual, phase by phase

```
  R (rejoiner)                                   S (coordinator) + survivors
  ────────────                                   ───────────────────────────
  ❶ re-derive identity (pkᵣ) from phrase
    build fresh MLS KeyPackage kpᵣ
    open reply queue Qᵣ
                          ◀──── recovery link (Qₛ, ticket) ── off-band ──
  ❷ seat_proof = sign(idₛₖᵣ,
       ticket ‖ kpᵣ ‖ republicId)
    ─── RecoverRequest{ member, pkᵣ, kpᵣ, ─────▶  ❸ verify seat_proof against the
        seat_proof, reply=Qᵣ } on Qₛ                  ANCHORED pkᵣ (from the chain
                                                       roster); spend the ticket
                                                    ❹ PROPOSE Membership{Restored,
                                                       member, pkᵣ} → chain
                                                       governance: survivors sign,
                                                       a block seals at m-of-n
                                                    ❺ on commit: restore_member(
                                                       member, kpᵣ) → (commit,
                                                       welcome); apply commit to
                                                       own group, BROADCAST the
                                                       raw commit over the mesh
                                                       to the other survivors
                          ◀── Welcome{ welcome, chain₀…ₕ₊₁ } on Qᵣ ──  ❼ serve the
  ❻ join MLS group from Welcome                        whole chain with the Welcome
    (now inside the encrypted group)                   (option A: the recovery
  ❽ VERIFY the whole chain from zero                   channel, no mesh needed)
    (sigs, links, threshold, own seat,
    genesis id = link id), materialize
    local workspace from it → current
```

**❶ Re-derive.** `R` derives its identity from its phrase (same `pkᵣ` as always),
builds a fresh `KeyPackage` from that key, and opens a reply queue `Qᵣ`.

**❷ Request.** `R` activates the link and sends a `RecoverRequest` to `S`'s
recovery queue, carrying its name, `pkᵣ`, the fresh `kpᵣ`, the **seat proof**
`make_seat_proof(idₛₖᵣ, ticket, kpᵣ, republicId)`, and `Qᵣ`.

**❸ Authenticate.** `S` verifies `verify_seat_proof(anchoredPkᵣ, ticket, kpᵣ,
republicId, sig)` — `anchoredPkᵣ` read from `S`'s **chain roster** (the genesis
identity table). A pass proves `R` holds the seat's identity key (only the phrase
re-derives it) and binds this exact fresh `KeyPackage` and republic. The ticket
is spent; a bad or replayed proof is dropped without a trace.

**❹ Re-admit (threshold).** `S` proposes a `Membership{Restored, member, pkᵣ}`
change; the survivors approve it over the mesh and a block seals at m-of-n
(`persistent_chain.md` §7). The block records, tamper-evidently, that the group
re-admitted `R` at height `H+1`. Because `pkᵣ` equals the already-anchored key,
the roster identity is unchanged — the block re-keys only the **MLS leaf**, not
the roster.

**❺ MLS re-key.** When the Restored block commits, `S` runs
`restore_member(member, kpᵣ)` → `(commit, welcome)`: an inline Remove(`R`'s stale
leaf) + Add(`R`'s fresh leaf) in one MLS commit. `S` applies the commit to its
own group and **broadcasts it raw over the runtime mesh** to the other survivors
(§6: a handshake frame riding the ordered per-link stream, so every group
advances past the same commit before any new-epoch traffic — one coordinator,
all apply, like the founder at founding); the **Welcome** goes to `R` on `Qᵣ`.

**❻ Rejoin the group.** `R` processes the Welcome and is back inside the
encrypted group — it can decrypt live traffic again.

**❼–❽ Catch up (option A: over the recovery channel).** The Welcome carries the
coordinator's **whole chain from genesis** — `R` has no mesh links yet, so the
recovery channel doubles as the catch-up channel. `R` **verifies the entire
chain from zero** — signatures, links, threshold, its own `(name, key)`
anchored, the genesis id equal to the seat-proof-bound link id (an untrusted
deliverer is safe). It then materializes its local workspace from the verified
chain and is current, holding the identical constitution + state as every
survivor — with an empty mesh; re-meshing is the separate *dynamic mesh
membership* feature (the mesh-based `ChainRequest{from: 0}` path from
`persistent_chain.md` stays available once meshed).

---

## 5. What the ritual guarantees

`R`, when finished, has verified — not trusted — that:

1. **It is a real member.** Its own `(name, pkᵣ)` appears in the genesis roster,
   and the whole chain verifies from block 0.
2. **Nothing was forged in its absence.** Every block carries m distinct member
   signatures over its exact change at its exact height; the links are unbroken.
3. **Its re-admission is on the record.** The `Membership{Restored}` block that
   let it back in is itself threshold-signed and in the chain.

The group gets the complementary guarantees:

1. **Seat ownership was proven.** Only the phrase-holder could sign the seat
   proof against the anchored key; the single-use ticket bound the request to
   the off-band grant.
2. **Re-admission was authorized.** It took m-of-n to seal the Restored block —
   no single survivor re-admitted `R` alone.
3. **`R` re-keyed cleanly.** Its stale MLS leaf was removed and its fresh one
   added in one coordinated commit; no ghost leaf lingers.

---

## 6. Load-bearing invariants — do not weaken them

- **Same identity, fresh MLS credential.** `R` re-derives the *same* identity key
  (so `pkᵣ` in the Restored block equals the anchored roster key — a verifier
  rejects a block that re-keys the roster identity to a different key), but a
  *fresh* `KeyPackage` (a new MLS leaf). Recovery re-keys the leaf, never the
  roster identity. (A deliberate identity **rotation** — a different `pkᵣ` — is a
  separate, out-of-scope concern.)
- **Threshold-gated re-admission.** Re-admission is a `Membership{Restored}`
  block that needs m signatures. A survivor that verifies the seat proof may
  *propose* re-admission; it cannot grant it.
- **Ticket-bound, single-use, ephemeral.** The recovery link's ticket binds the
  request (seat proof) and is spent on first use; an abandoned recovery leaves no
  trace and re-uses nothing.
- **Verify-from-genesis.** `R` re-verifies the whole chain from block 0 before
  trusting any state — a survivor that served a doctored chain is caught by the
  signatures and links, so an untrusted deliverer is safe.
- **One coordinator applies the MLS commit.** The re-key commit is a single MLS
  group operation every member must apply in the same order; `S` produces it and
  distributes it, exactly as the founder builds the group once at founding —
  never two members re-keying the same seat concurrently (that forks the group).

**MLS distribution — RE-decided (2026-07-09, supersedes the 2026-07-08 star):
the commit rides the RUNTIME MESH.** `S` broadcasts `restore_member`'s commit to
the survivors as a `WorkspaceEvent::MlsCommit` over the existing mesh — sent
**raw** (an MLS handshake frame, not application-encrypted: a commit wrapped at
the old epoch could never be processed), riding the per-link ordered stream so
survivors apply it *before* any new-epoch traffic. Ordering comes free with the
mesh; a star would split commit and follow-on chat across two channels and need
cross-epoch buffering. Only the **Welcome (+ the full chain, option A)** goes
over the dedicated recovery queue to `R`, which has no mesh links yet.

Two sender-side orderings are load-bearing here (both pinned by E2E tests):
the coordinator registers the pending recovery **before** proposing (a lone
m=1 self-cosign coordinator commits synchronously inside the propose), and
`adopt_committed_block` records the `Committed` envelope **after** the re-key's
`MlsCommit` (the outbox encrypts lazily at send time — a Committed sequenced
before the raw commit would reach still-old-epoch survivors as undecryptable
new-epoch ciphertext and be silently lost).

---

## 7. Real vs. simulated

Like the founding, the **product** recovers for real over the configured
transport; an offline **test seam** drives the rejoiner side over the in-process
loopback hub so the coordinator-side re-admission has a fast two-instance test.
The product never uses the seam.

---

## 8. Implementation map (status as of 2026-07-09)

- **Seat-proof crypto** — ✅ `crates/molt-engine/src/founding.rs`
  (`make_seat_proof`, `verify_seat_proof`, `seat_proof_bytes`).
- **MLS re-key** — ✅ `crates/molt-net/src/mls.rs` (`restore_member` →
  `(commit, welcome)`; `join_from_welcome`). Unit-tested end to end.
- **Threshold re-admission** — ✅ `crates/molt-engine/src/chain.rs`: the
  `Membership{Restored}` producer (`propose_membership`), the coordinator's
  seat-proof→propose decision (`verify_and_propose_restore`), and the commit
  trigger that runs the re-key on a committed Restored block (`coordinator_rekey`).
- **Catch-up + genesis adoption** — ✅ `crates/molt-engine/src/chain.rs`
  (`receive_block` headless-genesis, `request_catchup`, `serve_chain_from`) +
  `recovery.rs::sealed_roster_from_genesis`.
- **Recovery wire vocabulary** — ✅ `RecoveryInvite` link + `RitualMsg::Recover`
  (request) + `RitualMsg::Welcome` (re-admit) in `molt-net`/`recovery.rs`.
- **Coordinator link-mint** — ✅ `Command::RecoverInviteStart` (co-equal tool) →
  `cmd_recover_invite_start` → `recovery::spawn_recovery_provisioning` (mints the
  dedicated recovery queue on the runtime transport, wires the recv loop, renders
  the link) + the spend-once ticket guard in `cmd_net_recover_requested`. Proven:
  `two_instances.rs::recovery_flows_over_a_coordinator_minted_link`.
- **Rejoiner driver** — ✅ `recovery.rs::run_rejoin` / `rejoin_over_smp`
  (re-derive identity → fresh KeyPackage → `RecoverRequest` → await `Welcome` →
  `join_from_welcome`). Proven with real crypto (post-rekey bidirectional
  decryption) + two authentication tests (wrong phrase, doctored link) in
  `two_instances.rs`.

- **Coordinator distribution** — ✅ `coordinator_rekey` (chain.rs): on a
  committed `Restored` block it records the raw `MlsCommit` broadcast (mesh, §6),
  `spawn_welcome_send`s the Welcome **+ the full chain** to the rejoiner's reply
  queue (option A catch-up over the recovery channel), and posts the "🔑 …
  rejoined" group-chat notice after the commit.
- **Rejoiner chain verification** — ✅ `run_rejoin` hard-verifies the served
  chain from block 0 (`verify_served_chain`: signatures, links, threshold,
  genesis id = the seat-proof-bound link id, own `(name, key)` anchored) and
  returns `RejoinOutcome{…, chain, sealed}`.
- **Rejoiner lifecycle + materialize (A2)** — ✅ co-equal
  `Command::RecoverStart{link, phrase}` (MCP tool `recover_start`) runs
  `rejoin_over_smp` off the actor; the INTERNAL `NetRecoverSealed` re-verifies
  everything on the actor (defence in depth) and materializes the recovered
  workspace, adopting the **full** chain (`materialize_workspace`'s
  `full_chain` param) with an empty mesh (option A). `NetRecoverFailed`
  surfaces failures.
- **Proven end to end** — ✅ `two_instances.rs`:
  `recovery_completes_end_to_end_and_the_rejoiner_materializes` (1-of-2, the
  lone coordinator's self-cosign commits; mint → real seat-proofed request →
  commit → re-key → welcome + chain → the rejoiner's fresh device materializes
  and enters) and `recovery_distributes_the_rekey_commit_to_a_live_survivor`
  (1-of-3 with a live survivor supervisor: the broadcast raw commit merges and
  the post-re-key chat notice decrypts at the survivor).

**Still open (deferred, not recovery-blocking):**
- **Rejoiner mesh re-establishment** — the recovered workspace has no live mesh
  links (option A). Re-meshing = the general *dynamic mesh membership* feature
  (the runtime supervisor has a fixed peer set; changing membership rebuilds via
  `build_real_net`, as reopen does) — also needed for adding new seats.
- **Cross-epoch chat retry** — the MLS recv path drops wrong-epoch messages
  (no reorder buffer): in the lone-coordinator commit burst the ephemeral
  Proposed/Approved gossip can be lost to survivors (the committed block
  supersedes it), and a chat racing ahead of the commit is dropped. Hardening =
  classify wrong-epoch decrypt errors → don't-ack → redelivery; touches the
  delicate ack discipline, do deliberately.
- **Recovery UI** — the minted link surfaces only as `session.notice`
  (`recovery-link:…`), the rejoin flow as `recover-started:` / `recovered:` /
  `recover-failed:` notices; a real GUI surface + a distinct system-message
  style for the rejoin notice are open.
- **Ticket persistence + coordinator failover** — the spend-once ticket set and
  `pending_recovery` are in-memory; a coordinator crash after the Restored block
  commits leaves no one to re-key.

The state model this completes is `persistent_chain.md` (Phase 4); the founding
it mirrors is `founding_ritual.md`.
