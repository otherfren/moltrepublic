# Recovery approval: the survivors' vote gets a surface, the rejoiner co-signs

**Status: IMPLEMENTED 2026-08-08 (same session as the ratification), green on
master. Decision (1) — human approval of the membership card — was
SUPERSEDED 2026-08-23 by `recovery_auto_approval.md`: a consented restore
auto-approves on every survivor that verifies the consent itself; the card
stays as the visible record. Decisions (2) and (3) stand.** Keystones: `chain::tests::{a_consented_restore_seals_at_m_equals_n,
consent_abuse_rejects_the_chain, a_membership_proposal_is_a_visible_approvable_record}`,
the 2-of-2 `nostr_recovery` capstone, and the migrated `two_instances`
recovery suite. §2's settle runs in `after_block_applied`
(`settle_membership_records`), not the Committed apply arm.
Product decisions (user, 2026-08-08): (1) membership proposals appear as
normal proposal cards and are approved/declined like any proposal; (2) the
rejoiner automatically co-signs its own re-admission; (3) the engine refuses
to found a republic with threshold m < 2.

## 1. The defect this fixes

A verified `RecoverRequest` becomes `propose_membership(Restored)` — but that
registers the change only in the chain bookkeeping (`proposal_changes` +
`pending_sigs`). No `ProposalRecord` is created, so the proposal is invisible
to the GUI and MCP, and `cmd_approve` answers `UnknownProposal` for its id.
The only recovery that ever completed was m=1 with `self_cosign` (the
capstone's path). The rejoiner-side note ("waiting for the surviving members
to approve") described a flow that did not exist. With m ≥ 2 enforced, every
recovery would hang for 15 minutes and fail honestly.

Second, structural limit (named in the capstone comment): at m = n the lost
seat's own signature would be needed to re-admit it — a 2-of-2 republic could
never recover a seat. Decision (2) closes exactly this.

## 2. Membership proposals become visible proposals

One choke point, deterministic on every node and on replay: the **event
applier** (`events.rs`). The `MembershipProposed` arm — today a no-op —
creates the `ProposalRecord`:

- `surface: Organization` (membership is org governance; **no new `Surface`
  variant** — `Surface::ALL` feeds the checkpoint state layout, and a new
  variant would force a `molt-chain-checkpoint-v7` bump for a display
  concern).
- `payload: {"op": "restore_member", "member": <name>}` — same shape the
  proposal cards already render from; the GUI adds a label for the op.
- Insert **only if the id is absent** (idempotent re-gossip, replay over
  snapshots).

State transition: `after_block_applied` — where surface records already flip
on their block — gains the same flip for `Membership` blocks. A Membership
block carries no proposal id, so the open membership record is matched **by
content** (op + member), the same by-content cleanup the Checkpoint arm uses;
its state becomes `Applied` and the matching `pending_sigs`/
`proposal_changes` entries are dropped on every node (today only the sealer
cleans, by id). Restart/replay behavior is therefore identical to surface
proposals — one pattern, not two.

Approve/decline need no new commands: `cmd_approve` finds the record, and in
chain governance `chain_sign_and_gossip_approval` already resolves the id
through `proposal_changes` first — it signs the MEMBERSHIP bytes, not an
`Applied` payload. `cmd_decline` works off the record generically.

Guard adjustment: `id_free_for` treats "this exact membership change is
already registered under this id" as idempotent BEFORE refusing on record
existence — otherwise the record the applier just created would make the
legitimate wire re-gossip of the same proposal read as a collision. The
security property is unchanged: an id naming a DIFFERENT change or a foreign
surface proposal still refuses.

## 3. The rejoiner's consent (auto-co-signature)

The rejoiner cannot sign `approval_bytes` — those are position-bound
(`republic_id ‖ height ‖ change`) and the height is unknowable at request
time (governance keeps moving; re-bases re-sign). Instead the request carries
a **consent signature** over height-independent content, and the block
carries it inside the change, where every survivor's position-bound signature
covers it.

- Preimage (versioned, length-prefixed — never separator-joined):
  `"molt-restore-consent-v1\0" ‖ len(republic_id) ‖ republic_id ‖
  len(member) ‖ member ‖ len(identity_pk) ‖ identity_pk ‖
  (0 | 1 ‖ len(nostr_pk) ‖ nostr_pk)` — signed with the seat identity key
  (the phrase-derived Ed25519 the roster anchors). `nostr_pk` is the
  CANONICAL form of the new transport anchor (empty on loopback), the same
  value the change will carry.
- Wire: `RecoverRequest` gains `consent: String` (hex, additive
  `#[serde(default)]` — an old rejoiner sends none and keeps today's
  m-survivor behavior).
- Chain: `ChainChange::Membership` gains `consent: Option<String>`
  (additive). `approval_bytes` extends its conditional-extension scheme:
  marker byte `2` + counted signature bytes, appended only when present —
  every pre-consent block's preimage stays byte-identical (no tag bump; the
  byte-pin tests prove it).
- Counting rule, enforced identically in the **sealer** (`try_commit`) and
  the **verifier** (`verify_next`, shared by full and suffix walks): for a
  `Membership{Restored}` block whose consent verifies against the restored
  member's ANCHORED `identity_pk`, that member counts as one distinct
  signer. It must not also appear in `sigs` (the normal distinctness rule).
  Consent on any other change (or op) is rejected — hard, at the verifier.
- The coordinator validates consent at ingest (`cmd_net_recover_requested`):
  present-but-invalid drops the request (fail-closed, ticket unspent);
  absent proposes without consent.

Why height-independence is sound here: the consent authorizes "re-admit ME
with THIS identity and THIS transport anchor" — idempotent content, not a
position claim. Replaying it at another height still requires m−1 fresh
position-bound survivor signatures, and re-admission of a member already in
the roster does not change the roster (it re-keys the MLS leaf). The
single-use ticket separately gates the REQUEST path.

Scope note: consent deliberately does NOT cover the relay declaration — the
engine may substitute the ratified pool when the declaration is empty, and
relays are the survivors' governance (they sign them position-bound). The
seat proof already bound the rejoiner's declared relays at ingest.

## 4. m ≥ 2 at the engine

`cmd_create_propose` refuses `rule_m < 2` ("threshold below 2 - a republic
needs at least two voices"), so MCP and GUI meet the same gate (co-equality).
The GUI stepper already clamps to 2.

**Deliberately NOT in the verifier** (`verify_genesis` keeps accepting
m = 1): the genesis is immutable, and a verifier-side rejection would brick
every existing m=1 republic into the silent-legacy trap — the exact failure
mode backlog item 0 documents. Old republics keep working; new ones cannot
be founded below 2.

With (2)+(3): a 2-of-2 republic recovers with one survivor approval plus the
rejoiner's consent. m = n stays recoverable for every n. The recovery
capstone moves from 1-of-2 to 2-of-2 — proving the consent path end to end —
and a second test drives a survivor's `Command::Approve` through the public
surface (the record from §2).

## 5. Test plan (red first)

1. **Core/engine bytes:** consent extends `approval_bytes` only when present
   (byte-pin: a consent-less Membership preimage is byte-identical to
   today's), and the consent preimage is length-prefixed + versioned.
2. **Verifier:** a Restored block with m−1 survivor sigs + valid consent
   verifies; forged consent (wrong key, wrong member, tampered anchor)
   rejects the chain; consent on `Joined`/`Applied`/`Checkpoint` rejects;
   the restored member appearing in BOTH consent and `sigs` rejects
   (distinctness).
3. **Sealer:** `try_commit` seals at m−1 sigs + consent; without consent it
   keeps waiting for m.
4. **Surface:** after a verified request, every member holds a
   `ProposalRecord` (op `restore_member`); `Command::Approve` on it signs
   and seals; the record flips `Applied` on commit (sealer, passive nodes,
   replay); `Command::Decline` can reject re-admission.
5. **E2E:** the capstone at 2-of-2 (consent + coordinator approval); a
   2-of-3 run where the THIRD member approves via the public command
   surface while the coordinator only proposes.
6. **Founding gate:** `CreatePropose` with m=1 refuses with the exact
   message; existing m=1 chains still verify (adoption unaffected).
