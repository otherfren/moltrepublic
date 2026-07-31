# N3 execution plan — NIP-EE mapping + commit lifecycle

Status: **IN BUILD (started 2026-07-31).** Executes the N3 etappe of
`nostr_transport_marmot.md` §11 on top of the N2 relay runtime
(`nostr_n2_plan.md`). Every design input is ratified; this is the execution
map, not a discussion.

## 0. Scope

IN: the crypto/wire edge between the engine's `EventEnvelope` traffic and
the relay runtime — the 445 group event (build + strict parse), the outer
sealing under the exporter secret (§10.11), the exporter ring (§10.4, K=3),
the deterministic h-tag (§4.4), the 444 Welcome gift-wrap (NIP-59), the
explicit commit lifecycle, and the `ChainOracle` seam's contract.

NOT in N3: the ritual over Nostr (N4), the engine's `TransportKind` fork and
the runtime wiring (N4/N5), the `TransportPolicy` chain block (N6).

## 1. The fork bug we fix FIRST (mdk_evaluation.md §2.4)

`MlsMember::decrypt` merges any valid commit immediately; a losing same-epoch
commit fails `process_message` and is swallowed by the caller's discard arm.
**Two nodes that see concurrent commits in different orders diverge
permanently and silently.** Our exposure today is narrow (founder-only
commits at founding; `coordinator_rekey` at recovery) but real — two
concurrent recoveries for different members produce two same-epoch commits —
and it gets worse on an unordered multi-relay transport.

Fix, mirroring MDK's `CommitOrderingKey` but in OUR terms:

- A **total order over same-epoch commits** that every node computes
  identically from the commit's own bytes and its carrier event:
  `(created_at, event_id, commit_bytes_hash)` — timestamp first (matches
  §1's "lowest `created_at`, then lowest event id"), digest LAST so the
  order is not grindable.
- **Stage, don't merge, until the winner is known.** Within a bounded
  settle window a node holds the staged commit plus its ordering key; the
  lowest key wins, and a node that already merged a loser **rewinds to the
  prior state slot** and applies the winner.
- The loser's proposals are not lost: they are re-proposed at the new epoch
  (the chain layer re-decides, it never "replays" a merged commit).

TDD: the red test is two members committing at the same epoch with the
deliveries crossed; both must land on the SAME epoch state, and the test
must fail on today's code for the right reason (divergence, not an error).

**Review correction (2026-07-31, two independent CRITICAL findings).** The
first implementation passed its keystone while being broken in the WIRED
path: the test drove explicit-stamp variants nothing outside tests called,
while production stamped its own commit from a local clock and every
foreign commit with `0`. The own key could therefore never win — both
racers rewound onto each other's branch (still forked) and each silently
reverted the eviction it had just performed: worse than the pre-N3 discard
behaviour. The lesson is general: **a keystone that drives an API the
product does not use pins nothing.**

Corrections that landed:

- There is no wall-clock default anywhere. `restore_member` REQUIRES the
  stamp, and while the transport carries no per-event timestamp both sides
  pass `NO_CARRIER_STAMP`. Equal timestamps degrade the order to the digest
  alone — deterministic and symmetric (exactly one of two distinct commits
  hashes lower), only grindable, which is the honest cost of having no
  authenticated timestamp yet. N4's Nostr carrier supplies the real
  `created_at` to both ends and restores the timestamp-first order.
- **Bystanders converge too.** Only the two committers had a rewind slot, so
  every other member — the majority of any real republic — kept whichever
  commit arrived first and diverged permanently. The slot is now armed on
  EVERY merged commit, which gives every node the same rule.
- The keystone runs the PRODUCTION entry points (`restore_member` +
  `decrypt`), with two bystanders receiving the commits in opposite orders.

## 2. The outer envelope (§10.11 — current-Marmot raw AEAD)

`content = base64(nonce ‖ ChaCha20Poly1305(exporter_secret, plaintext,
aad = ""))`, one sealing keyed by the epoch's exporter secret itself. NOT
the older derived-keypair NIP-44 form §1 quotes. `exporter_secret =
group.export_secret("nostr", &[], 32)` (openmls `exporting.rs`).

**The exporter ring (K = 3, §10.4).** Epochs change only on membership and
recovery, so 3 covers the recent ones; the ACK/rewind layer covers the rest.
The ring holds OUTER-layer secrets only — the inner MLS layer still rejects
an evicted leaf's old-epoch message (`max_past_epochs = 0` stays). The
keystone pins exactly that asymmetry: **outer strips, inner rejects.**
Beyond the ring an event is epoch-opaque and reported loudly (G4), never
silently skipped.

## 3. The 445 group event

- Kind 445, ONE `h` tag (the rotated group tag), at most one `expiration`,
  **no other tag whatsoever** — the peeler's rule (`mdk_evaluation.md`
  §2.1), adopted with its 13-case rejection table: duplicate `h`, unknown
  tag, empty tag, valueless `h`, `h` with an extra element, uppercase hex,
  short hex, non-hex, duplicate/negative/overflowing `expiration`. Strict
  single-tag extraction counts OCCURRENCES, never first-match.
- **Recompute the event id before trusting it**, and **validate signature +
  shape BEFORE decrypting** (the peeler's order). N2 already verifies
  id+sig at the runtime edge; N3 keeps its own check so the envelope layer
  is safe against a future caller that does not.
- Published with a **fresh ephemeral key per event** (membership size stays
  hidden).
- `h_tag = KDF(rotation_seed, floor(unix/86400))`, uniform 24 h UTC windows,
  ±1 h skew margin (§4.4). The `rotation_seed` is a stable group secret set
  at founding and delivered in the Welcome — NOT the epoch-rotating
  exporter secret.

## 4. The 444 Welcome

NIP-59 gift-wrap to the invitee's transport anchor, deliberately UNSIGNED
inner rumor (a leaked 444 is not publishable). The fail-closed peel chain is
vendored from the peeler; `wrap_welcome_with_metadata`'s 32-byte KeyPackage
`e` tag is dropped with kind 443 (adaptation §2.1.3).

## 5. The `ChainOracle` seam

Contract as given in `nostr_n05_engine_inventory.md` §5: defined in
molt-net, implemented by molt-engine, handed into the runtime. N3 defines
the trait and the **drop-before-merge** rule (a group-data commit is
authorized by a threshold-decided chain block BEFORE `merge_staged_commit`,
never merge-then-reject) plus its hard-reject test.

**Honest status (review 2026-07-31): the seam is DEFINED, not WIRED.**
`ChainOracle` has no implementor and `authorize_group_data` no caller —
nothing today produces a group-data commit, so there is nothing to gate.
What exists is the contract and its refusal semantics, pinned by a test.
N6 supplies `ChainChange::TransportPolicy` and the engine-side implementor;
until then no code path can claim the gate protects anything.

## 5.5 Known debt carried out of N3 (review 2026-07-31)

Reported, not yet fixed — recorded so N4 does not build on a false
assumption:

- **The exporter ring is runtime-only.** A restart empties it, so
  cross-epoch catch-up falls back to the ACK/rewind layer until the node
  has seen K epoch changes again. Persisting it is a snapshot-format
  change; decide it with N4's transport-state work.
- **The outer envelope binds no context** (`aad` is empty): a sealed frame
  is not tied to its `h` tag, kind, or event id, so a group member could
  replay one group event's ciphertext into another position. The inner MLS
  layer still authenticates the sender and epoch, which bounds the damage
  — but the binding belongs in the AAD.
- **The ±1 h skew margin of §4.4 is documented and not implemented.** The
  h-tag derivation has no tolerance at a window boundary yet.
- **A live `Subscription` filter is immutable** (N2), so nothing follows
  the 24 h h-tag rotation across a boundary; N5's runtime must resubscribe
  per window.
- **The rewind is in-memory only.** A crash inside the MLS persist debounce
  can resurrect the losing branch. The race window is short and the
  delivery guarantee absorbs the traffic, but the interaction is unpinned.

## 6. TDD order

1. **Concurrent-commit fork** (§1) — red test first, then the ordering key
   + settle window + prior-state rewind.
2. Exporter access + the ring; **outer-strips/inner-rejects** keystone.
3. Envelope seal/open roundtrip + byte fixtures (our own — no Marmot
   interop, §10.3).
4. 445 build + the strict parse table (the 13 rejection cases).
5. h-tag derivation: window boundary, skew margin, offline re-derivation of
   missed windows.
6. 444 gift-wrap roundtrip + the fail-closed peel chain.
7. `ChainOracle` trait + drop-before-merge hard-reject test.
