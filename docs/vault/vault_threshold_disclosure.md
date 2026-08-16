# Vault: threshold-elected disclosure

Status: DRAFT (2026-08-16) — algorithm design for discussion. Nothing of the
cryptography is built; the only thing following this document so far is the
GUI design mock (`surfaces.slint::VaultPane`). Open questions in §10 must be
discussed with the user before any build phase starts.

## 1. Product idea

Members deposit secret data (passwords, private keys, recovery material) into
the republic's vault as **succession insurance**: if the depositor dies or
disappears, the DAO must not die with them. But the vault is not a shared
folder — nobody can read a deposit by default. Access is granted by a
**threshold vote that elects ONE member as the reader**; after the vote, only
that member can decrypt the secret. Everyone else — including every voter —
learns only *that* the grant happened, never the content.

"Zero-knowledge" here means, precisely:

- Below the threshold, **nothing** leaks — not to a single member, not to any
  coalition smaller than m, not to relays (information-theoretic for the key
  shares, AEAD for the payload).
- At the threshold, the honest protocol reveals the secret **only to the
  elected reader**, and every step is verifiable — a cheating depositor or a
  cheating share-holder is *detected and named*, not silently tolerated.
- This is threshold cryptography with verifiable secret sharing, not a
  ZK-proof system; the label describes the user-visible property.

## 2. Trust model — the honest limits

- **m colluding members can always reconstruct any secret**, vote or no vote.
  This is inherent to *every* threshold scheme: the coalition jointly holds
  the key. It is exactly the trust model the chain already runs on (m
  colluders can also forge blocks). The vault adds no weaker link.
- **Knowledge cannot be revoked.** Once granted, the reader knows the secret
  forever. There is no "one-time view" that cryptography can enforce; the UI
  must not pretend otherwise.
- **If more than n−m seats are permanently lost, the vault is lost** — the
  same availability bound as chain governance. That is the point: the secret
  survives any minority loss, including the depositor's death.

## 3. Primitives (all already pure-Rust posture, see §7)

| role | primitive | crate |
|---|---|---|
| payload encryption | XChaCha20-Poly1305, random 256-bit DEK | `chacha20poly1305` (in tree) |
| key sharing | Shamir over the Ristretto scalar field, **Feldman-verifiable** | `vsss-rs` (new) |
| per-seat share transport | HPKE base mode (X25519-HKDF-SHA256 + ChaCha20-Poly1305) | `hpke-rs` (in tree) or `hpke` — spike decides |
| vault keypair | X25519, derived `HKDF(identity_seed, "molt-vault-x25519-v1")` | `x25519-dalek`, `hkdf` (in tree) |
| bindings/ids | SHA-256 over length-prefixed canonical bytes | in tree |

The **vault key** is a dedicated X25519 keypair per seat, deterministically
derived from the identity seed (clean domain separation instead of
Ed25519→X25519 conversion of the roster key). Each seat announces its vault
public key ONCE, signed by its roster Ed25519 identity key; every member
verifies and pins it (persistent state). Because it is seed-derived, a
recovered seat re-derives the secret key and regains access to all its
shares from replicated state — recovery needs no vault-specific ceremony.

## 4. Deposit ("seal")

Depositor-local, ephemeral until the proposal commits (chain rule):

1. Sample scalar `s ← Z_l`; `DEK = HKDF-SHA256(s, "molt-vault-dek-v1")`.
2. `payload_ct = XChaCha20-Poly1305(DEK, payload, aad = header bytes)`.
3. Feldman-split `s` into n shares at threshold m (one per seat, m = the
   republic threshold); publish the Feldman commitments `C_0..C_{m-1}`.
4. For each seat i: `enc_share_i = HPKE_seal(vault_pk_i, share_i,
   aad = secret_id ‖ seat_i)`.
5. The sealed bundle `{header, payload_ct, commitments, enc_share_1..n}` is
   the body of a **`seal_secret` proposal** (Vault is a gated surface —
   deposits ride the existing threshold governance, which also stops spam).

`secret_id` = SHA-256 over `molt-vault-secret-v1` canonical bytes: tag,
republic id, depositor seat, name, kind, payload_ct hash, m, n, commitments —
every field le32-length-prefixed and entry-counted (the injectivity rule from
the republic-id fix; never separators).

**Approval doubles as share verification.** A member approves the proposal
only after (a) HPKE-opening its own share and (b) checking it against the
Feldman commitments. The approval signature (existing position-bound chain
signature) thus attests "my share is valid". Consequences:

- At m approvals the block commits: m *proven-good* shares exist, so the
  secret is **releasable** from that moment — no extra ceremony.
- The remaining seats verify on catch-up; the card shows "verified k of n"
  until k = n ("fully hardened"). A share that fails verification flags the
  secret visibly (`share invalid`) and names the depositor — detected, never
  silent.
- A denied proposal discards the bundle; nothing was persisted (ephemeral
  until commit, like the ritual).

Members store their decrypted share only inside the existing at-rest-sealed
local store — and can always re-derive it from the replicated bundle.

## 5. Grant (the vote) and unseal (the re-encryption)

1. **Request:** any member proposes `VaultGrant { secret_id, reader,
   reason }` — an additive `ChainChange` variant, so the vote IS the existing
   chain governance: position-bound signatures over
   `republic_id ‖ height ‖ change`, sealed deterministically at m.
2. **Unseal:** on seeing the committed grant block, every share-holding seat
   computes `resp_i = HPKE_seal(vault_pk_reader, share_i,
   aad = "molt-vault-resp-v1" ‖ republic_id ‖ secret_id ‖ grant_height ‖
   reader ‖ seat_i)` and publishes it on the group channel. The AAD binds the
   response to THIS grant and THIS reader — a response cannot be replayed for
   another grant or redirected to another recipient.
3. **Read:** the reader HPKE-opens incoming responses, verifies each share
   against the public Feldman commitments (a bad or missing responder is
   thereby *identified by seat*), Lagrange-combines any m valid shares → `s`
   → DEK → decrypts `payload_ct` locally. The plaintext is displayed, never
   persisted — it is re-derived on demand from the persisted responses, so
   the grant survives restart and recovery.

Everyone else's client shows the grant as an audit entry — name, reader,
height, time — with the content permanently locked. That contrast (my entry
opens, the neighbour's entry shows only who may read it) is the UI's
zero-knowledge story.

What a cheater can and cannot do:

- **Depositor** distributing bad shares → caught at approval (§4); with < m
  approvals the secret never enters the vault.
- **Share-holder** sending garbage at unseal → the reader's commitment check
  names the seat; any m honest responses suffice.
- **Voter coalition < m** → grant never commits, shares never move.
- **Re-routing**: approvals are position-bound chain signatures naming the
  reader; responses are AAD-bound to grant and reader. Neither can be
  spliced onto a different reader.
- **Out-of-band collusion ≥ m** → reconstructs regardless (§2) — no scheme
  allowing m honest members to release can prevent m dishonest ones.

## 6. What was considered and rejected

- **Threshold encryption with a DKG** (threshold ElGamal / BLS,
  `threshold_crypto`-style): one shared public key, no per-secret share
  distribution. Rejected: needs a DKG ceremony and resharing machinery on
  membership change — but this product FIXED n and m at founding forever
  (product decision 2026-07-11), which removes the one advantage; and it
  drags in pairing crypto. Per-secret Shamir is smaller, and its shares ride
  infrastructure we already run.
- **Plain Shamir without VSS** (`sharks` et al.): a lying depositor could
  hand out inconsistent shares and nobody would know until the release
  fails. Verifiability is the point; rejected.
- **Delivering shares as MLS group messages**: the group channel is readable
  by ALL members — shares must be per-seat confidential. Hence the HPKE
  envelope per seat *inside* the group-encrypted transport.
- **Feldman vs Pedersen**: Feldman commitments leak `g^s`; for a one-time
  random scalar fed through HKDF this is standard and harmless (DDH), and
  Feldman lets the READER verify shares against the same commitments the
  depositors published. Pedersen hides `g^s` but breaks that direct check.
  Feldman, with the tradeoff recorded here.

## 7. Library verdict (don't hand-roll — checked 2026-08-16)

- **`vsss-rs` 6.0.1** — "Verifiable Secret Sharing Schemes", pure Rust,
  no-std, Shamir + Feldman + Pedersen over `elliptic-curve`/curve25519
  groups; actively maintained (last release 2026-07-31), ~1.7M downloads.
  The ONE new dependency. A dep-lock spike (wallet-plan §6 pattern) locks
  its exact API and audits its dependency slice before build start.
- **HPKE**: `hpke-rs` 0.6.1 is ALREADY in the tree (OpenMLS's HPKE). Prefer
  reusing it; fallback `hpke` 0.14.0 (RFC 9180, pure Rust, ~7.7M downloads)
  if hpke-rs's public API turns out unergonomic outside OpenMLS. Spike
  decides; never hand-rolled X25519+AEAD.
- `chacha20poly1305` 0.10, `x25519-dalek` 2.0, `hkdf` 0.12,
  `curve25519-dalek` 4.1 — all already in the lockfile.
- Everything pure Rust: the ring-free guard and the no-C posture hold.

## 8. Engine and state integration (sketch)

- Vault-key announcements, sealed bundles, and grant responses are
  replicated persistent state; bundles and grants are chain blocks
  (`ChainChange` additive-only). All of it must survive WP4a compaction and
  ride the checkpoint like other applied projections — an integration point
  the build phase pins with tests, byte-layout tags versioned
  (`molt-vault-*-v1`) with byte-pin tests like roster/checkpoint.
- Engine stays a single-owner actor: HPKE/Shamir work is CPU-only and cheap,
  so seal/verify/combine run inside command handlers; no new async pattern.
- Co-equality: new commands surface as MCP tools — `vault_seal`,
  `vault_request` (the grant proposal), `vault_read` (reader-side reveal),
  vault lists in `read_state`; approvals reuse the existing `approve`.
  Net-side response ingestion is INTERNAL.

## 9. Build phasing (after ratification — nothing started)

1. **V1 spike**: dep-lock `vsss-rs`, decide `hpke-rs` vs `hpke`, red
   byte-pin tests for the three `molt-vault-*-v1` layouts.
2. **V2 seal**: vault-key announcement, `seal_secret` proposal with
   verify-at-approve, secrets list real (TDD: two-instance seal keystone).
3. **V3 grant**: `VaultGrant` chain variant, response task, reader reveal,
   audit list (keystone: three nodes, elected reader decrypts, non-reader
   provably cannot).
4. **V4 polish**: MCP tools, recovery/compaction keystones, UI de-mock.

## 10. Open questions (the discussion this document starts)

- **Unseal quorum off by one?** The elected reader holds a share too: m
  shares INCLUDING the own one means m−1 answers suffice — the mock's
  meter currently waits for m answers from the n−1 other seats, so one
  dead seat would show every grant "pending" forever (exactly the
  succession scenario). Decide the counting rule before the meter is
  real.

1. **Threshold shape.** Recommended: ONE threshold — the republic's m — for
   deposit, vote and reconstruction alike ("security comes from the
   threshold alone", no per-secret knobs). Alternative: per-secret k ∈
   [m..n] chosen at seal (the treasury seed at n-of-n) — buys depositor
   control, costs a veto by every dead seat, which cuts against the
   succession idea. Decide.
2. **Deposit gating.** Recommended: deposits are ordinary gated proposals
   (§4, approval = share verification). Alternative: ungated deposits with a
   separate n-of-n verification round — faster to seal, second ceremony.
3. **Response duty.** Recommended: EVERY seat responds to a committed grant
   (fastest completion; reveals nothing extra). Alternative: only approvers.
4. **Grant durability.** Recommended: a grant is permanent (honest — see
   §2); the audit entry says so. Alternative UX with expiring *display* only.
5. **Payload bound.** Vault payloads ride the transport publish budget, so:
   small secrets inline (cap ~a few KiB); anything larger goes into the
   existing encrypted file plane with the FILE KEY as the vault secret.
   Confirm this split.
