# The Founding Ritual

How a MoltRepublic *republic* comes into being. This document describes the
ritual **abstractly** — the actors, the messages they exchange, the secrets
that bind them, and the guarantees that hold when it is over. It is transport-
and language-agnostic; the concrete wire (per-queue wrapping over the loopback
transport today; Nostr relays in build — etappe N4 of
`docs/transport/nostr_transport_marmot.md` re-implements the ritual wire) and
the code are pointed to at the end.

---

## 1. Principle: constitute, then create

A republic is a fixed group of *n* members with an *m*-of-*n* approval rule. It
is **constituted before it exists**. There are no open seats and no "add member
later": a workspace only ever comes into being with its **complete, sealed
member list**, signed by everyone, from its very first byte on disk.

Three properties fall out of this and shape the whole ritual:

- **Everyone signs everyone.** The final act is a *roster* — the ordered table
  of every member's identity key — that every member signs. The workspace's
  genesis carries all *n* signatures, so the membership is unforgeable from
  birth: no member can later be added, removed, or impersonated without
  breaking an existing signature.
- **No node is privileged.** The founder runs the ritual, but the republic's
  identity is derived from the *content* of the roster, not from the founder's
  keys. Symmetry is deliberate (see §5).
- **Ephemeral until sealed.** Nothing touches the disk until the last signature
  arrives. Abandon the ritual — cancel, navigate away, crash — and every
  distributed invite is void and the disk is untouched. A new attempt starts
  from scratch. (The session is in-memory: a founding that has not sealed does
  not survive an app restart.)

---

## 2. Actors and their keys

Every participant — founder and members alike — holds **their own secret
recovery phrase** and nothing else is shared a priori. From that phrase each
node deterministically derives:

- an **identity keypair** (Ed25519), used to sign the roster **and** as the
  member's MLS credential key (one identity, two anchors: the genesis table and
  the MLS `KeyPackage` carry the same key, and a verifier checks they match).
  It is per-member and re-derivable from the phrase alone after total device
  loss — never random, never persisted in the clear.
- the keys and identifier for that member's **own local workspace** (§6).

Crucially, a member's identity key is derived from *its own* phrase. The
founder never sees any member's private key; it only ever learns the public
key, and only when the member chooses to present it.

---

## 3. The invite

The founder mints **one single-use invite per future member** (one per seat;
the founder holds seat "self"). An invite is a `molt://invite/…` link with two
layers:

- a **preview** — republic name, *m*-of-*n*, inviter handle, a ticket stub —
  which any node can parse to show the user *what* they are joining; and
- a **transport handover** — where and how to reach the founder — appended as
  one opaque segment. A link that carries no handover is a preview only and is
  **not joinable** (it is rejected with a clear message).

The security core of the invite is the **ticket**: a high-entropy, single-use
secret. Activating an invite is cryptographically bound to its ticket by a MAC
(§4, step 2), so a bare leaked queue address cannot knock, and a replayed
ticket is rejected.

Approval during founding is **automatic**: a valid ticket+MAC turns a seat
"green" without a founder click — the founder distributed those links itself,
off-band, and is watching the list fill live; the single-use ticket carries the
trust. (Recovery joins, for an already-filled seat, stay manually approved —
out of scope here.)

---

## 4. The ritual, phase by phase

Below, `F` is the founder and `Mᵢ` is the member filling seat *i*. Every leg is
a message on a mutually-anonymous queue; nothing but these messages (and the
off-band invite links) is shared.

```
  F (founder)                                     Mᵢ (member i)
  ───────────                                     ────────────
  ❶ derive founder identity
    mint ticketᵢ, open invite queue Qinv
    publish invite linkᵢ  ───────── off-band ─────────▶  (paste link)

                                                    ❷ derive own identity (pkᵢ)
                                                       build MLS KeyPackage kpᵢ
                                                       open reply queue Qrepᵢ
                          ◀──── JoinRequest{ name, pkᵢ, kpᵢ, ─────
                                mac, reply=Qrepᵢ } on Qinv

  ❸ verify mac against ticketᵢ
    anchor identity {name, pkᵢ}, keep kpᵢ, seat green
    …wait until every seat's key is in…

  ❹ DELIBERATE: propose final name + agenda (charter)
    build table T = canonical(republicId(name), m, n, [identities], agenda)
    T ──────── Seal{ name, agenda, T } on each Qrepᵢ ─────▶
                                                    ❺ review name+agenda,
                                                       HUMAN confirms, then
                                                       sign T with own key
                          ◀──────── Signed{ sigᵢ } on Qinv ──
  ❻ verify sigᵢ against pkᵢ over T
    …wait until every seat has ratified…

  ❼ sign T itself, assemble all n attestations
    build MLS group, add every kpᵢ → one Welcome W
    write OWN Founded genesis (own seed) + persist own MLS state
    sealed = {name, republicId, m, n, roster, identities, attestations, agenda}
    sealed,W ─────── Genesis{ sealed, welcome=W } on each Qrepᵢ ▶
                                                    ❽ verify sealed:
                                                       · republicId = content
                                                       · n-of-n attestations ok
                                                         (over name+agenda too)
                                                       · own (name,pkᵢ) present
                                                       join MLS group from W,
                                                       persist own MLS state,
                                                       write OWN Founded genesis
                                                       (own seed) → enter republic
```

**❶ Open the ritual.** `F` derives its identity from its phrase, mints a fresh
single-use ticket per seat, provisions one invite queue `Qinv` it will *receive*
on, and publishes an invite link per seat (the link carries `Qinv`'s address,
its wrapping key, and the ticket). `F` shares each link off-band with the
intended person.

**❷ Activate.** `Mᵢ` derives its own identity `pkᵢ` from its own phrase, builds
an **MLS `KeyPackage`** from that same key (one identity, two anchors), opens a
**reply queue** `Qrepᵢ` it will receive on, and sends a `JoinRequest` to `Qinv`
carrying its chosen name, `pkᵢ`, `kpᵢ`, the ticket MAC, and the address+key of
`Qrepᵢ` (each party owns the queue it receives on — see §7).

**❸ Anchor.** `F` verifies `mac = HMAC(KDF(ticketᵢ), name ‖ pkᵢ)` against the
unspent ticket, spends the ticket, anchors `{name, pkᵢ}` and keeps `kpᵢ` for the
group. The seat turns green. A bad or replayed MAC — or a missing KeyPackage —
is dropped without a trace.

**❹ Deliberate & seal round.** Once *every* seat's key is in, the ritual does
**not** auto-seal. `F` proposes the **final DAO name** and a free-text
**agenda/charter**; `F` then freezes the **roster table** `T` — the one
canonical serialization of `(republicId(name), m, n, identities, agenda)` — and
sends `Seal{name, agenda, T}` to every member on its reply queue. Because the
agenda is inside `T`, a signature over `T` is a ratification of exactly this
charter (the name is bound too, via the republic id that salts `T`).

**❺–❻ Ratify.** Each `Mᵢ` sees the proposed name+agenda and — on an **explicit
human confirm** — signs `T` with its identity key and returns the signature; `F`
verifies each against the anchored `pkᵢ`. Until a member confirms, nothing
seals: the workspace opens only after *everyone* has ratified. The seat handler
is idempotent — a second, distinct signature for a seat is ignored, so one
member cannot inflate the roster.

**❼ Finalize & distribute.** When every seat has ratified, `F` adds its own
signature, assembles all *n* attestations, **builds the MLS group** by adding
every `kpᵢ` in one commit (producing a single `Welcome`), **writes its own
`Founded` genesis first** and persists its own MLS state (so a founder disk
failure cannot orphan members on a constitution the founder never persisted),
and only then distributes the complete `sealed` roster **and the `Welcome`** to
every member.

**❽ Everyone seals.** Each `Mᵢ` **verifies** the distributed roster (§8 — the
recomputed table now covers the charter, so a genesis whose name/agenda differs
from what was ratified fails verification), **joins the MLS group from the
`Welcome`** and persists its own group state, and writes its **own** `Founded`
genesis from the roster — under its **own** seed — then enters the republic.
Every member now holds the same constitution in its own encrypted workspace and
is already inside the group.

---

## 5. The republic id — neutral by construction

Everyone signs `roster_canonical_bytes(republicId, m, n, identities)`, so the
`republicId` used as the salt must be a value **every member computes
identically** — and, for symmetry, one that **no member's seed defines**.

The republic id is therefore **content-derived**:

```
republicId = SHA-256( "molt-republic-id-v1\0" ‖ name ‖ 0 ‖ m ‖ n
                       ‖ each identity public key, sorted and 0-separated )
```

Sorting the keys makes it order-independent; deriving it from the roster's own
content makes it independent of who the founder is. It is stored in the
`Founded` genesis so any member (or later auditor) can recompute it from the
roster and check that the attestations were signed over the right salt. It is
**not** a workspace's storage identifier — see §6.

---

## 6. One republic, one workspace per member

The republic is a single shared constitution, but **each member stores its own
encrypted copy**:

- The member's **local workspace id and encryption keys** are derived from that
  member's **own seed**. Two members of the same republic have *different* local
  workspace ids on disk — nothing about the storage layer is shared or
  correlatable.
- The **republic id** and the signed roster live *inside* each member's genesis
  as shared data (the roster salt and the membership proof), never as a shared
  on-disk identifier.

So "join a republic" ends with the joiner holding its own workspace, keyed by
its own seed, that verifiably encodes the same membership as the founder's.

---

## 7. Transport shape (abstract)

The ritual assumes only **unidirectional, mutually-anonymous message queues**:
the *recipient* creates a queue and hands its send-side address to exactly one
sender. Two consequences shape the message flow above:

- **Each party owns the queue it receives on.** The founder creates the invite
  queue (it receives joins and signatures there); each member creates its own
  reply queue (it receives the table and the sealed roster there) and advertises
  that queue's address inside its `JoinRequest`.
- **Every message is wrapped under a per-queue key** before it hits the wire.
  The purpose is *copy unlinkability*, not confidentiality: the *n−1* copies of
  one distributed message must be pairwise byte-distinct, or a server hosting two
  members' queues could link them into a group at a glance.

The ritual is written once against this queue abstraction, so the same
member-side and founder-side code runs over the in-process loopback hub (the
offline test seam) unchanged over any queue-shaped transport — it ran over
real SMP servers until the SMP transport was removed (etappe N-demo,
2026-07-30), and N4 carries it onto Nostr relays.

---

## 8. What the ritual guarantees

When a member finishes, it has verified — not trusted — that:

1. **The ticket bound the activation.** Only a holder of the (single-use,
   off-band) ticket could have activated the seat; a leaked queue address alone
   could not.
2. **The republic id matches the roster.** Recomputing it from the distributed
   identities yields exactly the stored value — no member's seed skewed it.
3. **Everyone signed.** There are exactly *n* identities and *n* attestations,
   and every attestation verifies against its member's anchored key over the one
   canonical table.
4. **It is actually a member.** Its own `(name, key)` pair appears in the sealed
   roster — a founder that excluded it, or anchored its key under a different
   name, is rejected and leaves it with no workspace.
5. **The charter is the one it ratified.** The recomputed table binds the DAO
   name and agenda, so a genesis whose charter differs from what the member
   signed fails verification — the founder cannot swap in a different charter
   after the fact.

The founder gets the complementary guarantee: every seat is filled by a holder
of a ticket it minted, and every member ratified the identical roster **and
charter**. And because every member's `KeyPackage` was added to the MLS group
before the `Welcome` went out, everyone ends the ritual sharing one group whose
credential identities are exactly the anchored keys.

---

## 9. Lifecycle & failure

- **Ephemeral.** No disk write happens before the final seal. Cancelling the
  ritual, navigating away from it, or crashing **voids** every distributed link
  and leaves the disk untouched; the founding is abandoned and its background
  work torn down so it can never seal and hijack the session later.
- **Provisioning failure** (the founder cannot reach the transport) fails the
  founding with a surfaced error rather than waiting forever for links that
  never appear.
- **Join failure** (bad roster, unreachable founder, disk error) surfaces into
  the join run as a retryable failure — never a silent hang.
- **Waiting is real.** Members join off-band on their own schedule; a founding
  legitimately waits (minutes, hours) for every link to be activated and signed.
  The wizard shows a real member list filling in, not a fake progress bar.

---

## 10. Wire vocabulary

One tagged message type rides the queues as ordinary payloads (inside the
per-queue wrap; inside MLS ciphertext once MLS lands):

| Message | Direction | Carries |
|---|---|---|
| `JoinRequest` | member → founder | seat, name, identity pk, **MLS KeyPackage**, ticket MAC, reply-queue handover |
| `Seal{ name, agenda, table }` | founder → member | the proposed charter (name + agenda) to review, and the canonical bytes to sign (hex) |
| `Signed{ sig }` | member → founder | the member's signature over the table (its ratification) |
| `Genesis{ sealed, welcome }` | founder → member | the complete sealed roster (name, republic id, *m*/*n*, roster, identities, all attestations, **agenda**) and the **MLS `Welcome`** |

The invite link's transport handover carries `{ server, invite-queue id,
wrapping key, seat }`; the member's reply-queue handover (inside `JoinRequest`)
carries the same shape for `Qrepᵢ`.

---

## 11. Real vs. simulated

The **product** always founds and joins for real over the configured transport.
A single offline **test seam** simulates the other members over the in-process
loopback hub — deterministic, network-free — so the founder-side sealing has a
fast test; the product never uses it.

---

## 12. Implementation map

- **Ritual driver & member side** — `crates/molt-engine/src/founding.rs`
  (`start_ritual`, `run_ritual_member`, `FoundingInvite`,
  `verify_sealed_roster`, `RitualRuntime`; the over-SMP drivers were removed
  in etappe N-demo — N4 adds the Nostr provisioning/join tasks).
- **Lifecycles (create/join wiring, materialize)** —
  `crates/molt-engine/src/lifecycles.rs`.
- **Crypto primitives** — `crates/molt-storage/src/lib.rs`
  (`derive_identity_key`, `identity_sign`/`identity_verify`, `republic_id`) and
  `crates/molt-net/src/invite.rs` (ticket, `join_mac`, `RitualMsg`).
- **Canonical roster + sealed-roster types** — `crates/molt-core/src/lib.rs`
  (`roster_canonical_bytes` binds the agenda, `MemberIdentity`,
  `RosterAttestation`, `SealedRoster`, the `Founded` event with its `agenda`).
- **Deliberation** — the founder's `CreatePropose{name, agenda}` and the
  joiner's `JoinConfirmCharter` / `Ratifier` gate (co-equal on both surfaces),
  in `founding.rs` / `lifecycles.rs`; the create + join wizard panels in
  `crates/molt-ui/`.
- **MLS group** — `crates/molt-net/src/mls.rs` (`MlsMember`: KeyPackage from the
  identity key, founder group create, Add+Welcome, join-from-welcome, app-message
  encrypt/decrypt, snapshot/restore into `transport.state.mls`).
- **Transport** — `crates/molt-net/` (the `Transport` trait, `wrap`, `chunk`,
  and the loopback hub in `src/loopback.rs`; the SMP client was removed in
  etappe N-demo, the Nostr transport is in build).
- **Proven end-to-end** — `crates/molt-engine/tests/two_instances.rs`
  (loopback: two engine instances found and join, the MLS group
  interoperates across instances, and the ratification gate holds until the
  joiner confirms). The over-a-real-SMP-server twin was retired with the SMP
  transport (etappe N-demo); N4's keystone is its Nostr twin.

The transport concept this realizes is `docs/transport/concept-transport-simplex-tor.md`
(§3.3).
