# Vault: majority escrow with an elected reader

Status: DRAFT rev 4 (2026-10-05), being built per
`vault_build_plan.md`. The product decisions in §2 were ratified by the
user in the 2026-10-05 discussion; the protocol below is the design they
imply. Rev 3 added D11–D13 after a second review the same day
(founding-only vault, keys in the roster, deposit is a vote, m ≤ n − 2)
and D14–D16. Rev 4 folds in the build plan's deviations (plan §1.3,
listed in §12) and D17 (plan Q1, user-confirmed default).
Built so far: the contract types (stage S0). None of the cryptography is
built yet.
Rev 1 (2026-08-16) is superseded: its primitives stand, its framing
("succession insurance" with an implied depositor protection) and its
storage story (bundles inline in the chain) do not. §12 lists what
changed and why.

## 1. What the vault is — and what it is not

A member deposits a short text (a last will, server passwords, API keys)
so the republic can still fulfil its purpose when that member is gone.
Nobody can read a deposit by default. **Any m members can, at any time,
vote ONE member in as its reader**; from then on that member can decrypt
it, and every other member sees only that the grant happened.

It is **not a dead man's switch** in the literal sense. A switch fires on
absence, and a republic cannot observe absence: humans and agents are
indistinguishable seats, and a seat can run forever with nobody behind it.
The trigger is the majority, not the silence. It is also **not a private
safe**: the depositor has no veto and no delay to object in. The vault
has exactly the power the republic's governance already has — whoever
holds m votes can sign blocks, and can open the escrow. The UI says so at
deposit time, in one line:

> Any m members can open this at any time - by vote for one of them, or among themselves. There is no way back.

## 2. Decisions (user, 2026-10-05)

- **D1 One reader.** A grant names exactly one member; only that member
  can decrypt.
- **D2 No veto, no delay.** A grant takes effect the moment it commits at
  m approvals.
- **D3 Humans and agents are the same.** No step anywhere in the vault may
  depend on telling them apart (no human-only confirmation, no liveness
  heuristic).
- **D4 Payload ≤ 100 KiB** (102 400 bytes of plaintext): a will and a few
  keys, not an archive.
- **D5 Every member may deposit, as many separate deposits as it likes**,
  each under its own name. All seats are equal.
- **D6 No way back.** A grant is permanent and knowledge cannot be
  revoked; the UI must not pretend otherwise.
- **D7 Replace drops the old version.** Depositing again under the same
  name replaces the deposit; honest clients drop the old version's
  material.
- **D8 Name and kind are visible** to all members — nobody votes blind on
  what they release.
- **D9 One threshold.** The republic's m, everywhere: approving a deposit,
  granting, reconstructing. Share refresh (§9.6) comes later, not in v1.
- **D10 The vault lives where the wiki lives**: in the chain on disk,
  folded at a cut, its bulk on the file plane — and **all of it is in
  every backup**. This decision surfaced a backup bug in the wiki itself:
  `wiki_base.bin` was skipped by every export (fix of 2026-10-05,
  "storage: back up the folded wiki base").
- **D11 Founding only, set up in the background.** The vault can be
  chosen only at founding; `set_features` refuses to add it later. Every
  seat's vault key is part of the founding (§6), so no seat can lack one
  and no step needs a user action.
- **D12 A deposit is a vote.** Deposit and replace are ordinary gated
  proposals at m-of-n, decided by members like any other change — not
  approved automatically. That vote is the only bound on how much every
  seat must hold (no count cap, no delete; replace only).
- **D13 m ≤ n − 2.** The vault tolerates at least one dead holder (§3).
- **D14 Older builds never get the vault.** No compatibility path: an
  older build can neither found nor join nor run a vault republic (§6).
- **D15 Re-seal after a complaint is offered, never automatic** (§7).
- **D16 A cut drops the grant audit of replaced versions.** Who read an
  old version disappears below the cut with that version (§9.3) — the
  checkpoint summarizes, it does not archive.
- **D17 A grant answered on a displaced branch stays audited locally**
  (plan Q1, user-confirmed default). D2 holds: holders answer at commit.
  A reorg above the anchor can displace a committed grant; every seat
  that applied it keeps a persisted local line `released, then
  displaced`, and answering is deduplicated by `grant_id`, so a re-vote
  that commits again is not answered twice.

## 3. When the vault can be enabled

The vault is a charter feature (`charter_features.md`), selectable at
founding only (D11), and the republic's shape decides whether it can
mean anything:

- **m ≥ 2.** At m = 1 Shamir hands every seat the secret itself: every
  member could read every deposit with no grant at all. A 1-of-n
  republic cannot enable the vault.
- **m ≤ n − 2 (D13).** The depositor holds no share (§6), so n − 1
  holders carry a threshold of m and the vault tolerates (n − 1) − m dead
  holders — one fewer than governance. At m = n − 1 that is zero: in a
  2-of-3 republic a grant commits with one seat dead, and the read it
  grants is impossible. So n ≥ 4.

Both are checked in the founding wizard, in `verify_seal_proposal`, and in
`set_features` (which refuses `vault` outright, D11); the refusal names
the reason (`needs 2 <= m <= n-2`).

## 4. Trust model — the honest limits

- **m members can always read any deposit.** By vote (the design) or out
  of band (m share holders pool their shares). No scheme that lets m
  honest members release can stop m dishonest ones. This is the trust
  model the chain already runs on.
- **Knowledge is irrevocable** (D6). After a grant, the reader knows the
  content forever. Rotating the actual passwords is the only real undo.
- **Availability equals the republic's.** If more than n − m seats are
  permanently lost (phrase gone), the vault is lost together with
  governance — no worse, and no scheme can do better without weakening m.
  The depositor's own loss is already priced in: it holds no share.
- **A replaced version survives until the next cut, and in backups
  forever.** Shares are re-derivable from a deposit record plus a seed
  (§9.4), and the old deposit block stays in the chain until a cut folds
  it away. What "drop" (D7) actually removes is the old payload file,
  and only at the next cut: above the anchor the chain can still reorg
  and make the old version current again. m members who kept it (or a
  backup holding it) can still read the old version. The UI says so when
  replacing.
- **The depositor's phrase opens its own deposits.** Dealing and the
  share ephemerals derive from the depositor's vault seed (§7), so that
  phrase recomputes every share it dealt. Not new: rev 3's seed-derived
  ephemerals already allowed it.
- **Chained false complaints drain the threshold.** Each reveal publishes
  one share; k colluding complainers lower a deposit to m − k (they could
  publish their shares out of band anyway). The card shows it, floored at
  0, and the re-seal offer restores m.
- **The share protection is computational.** Feldman commitments publish
  `g^s`; secrecy below the threshold rests on the discrete log (and HPKE on
  X25519), not on information theory. Rev 1 claimed otherwise.

## 5. Primitives (pure Rust, see §13)

| role | primitive | crate |
|---|---|---|
| payload encryption | XChaCha20-Poly1305 under a DEK derived from the shared scalar | `chacha20poly1305` (in tree) |
| key sharing | Shamir over the Ristretto scalar field, Feldman-verifiable | `vsss-rs` 5.1.0 (new) |
| per-seat share transport | HPKE base mode (X25519-HKDF-SHA256, ChaCha20-Poly1305) | `hpke` 0.13.0 (new; caller-supplied ephemeral) |
| vault keypair | `DeriveKeyPair(vault_ikm)`, `vault_ikm` below | `hpke` `X25519HkdfSha256::derive_keypair`, `hkdf` (in tree) |
| deterministic dealing | ChaCha20 from `deal_seed` (§7) | `rand_chacha` 0.3 (in lock) |
| ids and bindings | SHA-256 over length-prefixed, entry-counted canonical bytes | in tree |

**The vault key** is `vault_ikm = HKDF-SHA256(entropy, info =
"molt-vault-x25519-v1" ‖ founding_nostr_pk ‖ identity_pk)`,
`(vault_sk, vault_pk) = DeriveKeyPair(vault_ikm)`. The identity alone
would not do: a joined seat derives it from
`derive_workspace_id(entropy, "member")`, the SAME for every republic
(`founding.rs::seat_identity`), so one phrase would get one vault key
everywhere. The salt is the seat's FOUNDING `nostr_pk` (rev 3 said
`republic_id`, which is first computed after every join, so a joiner
could not derive the key in time for its join): it is ticket-salted with
a random ticket, so one phrase gets a different vault key in every
republic, and it is fixed in the genesis and in
`CheckpointState.founding_identities` for life, so a recovered seat (new
working `nostr_pk`, same phrase) re-derives the same key from the chain.

**`vault_ikm` is persisted** in `TransportState.vault_seed` at founding,
join, recovery and restore, because the phrase is not available at
runtime. A holder seat of a vault republic without it fails closed: it
refuses `approve` on the vault (`no vault key`), sends no receipt and no
answer, and logs `vault_seed=missing` once per open; it re-derives at
the next phrase-bearing open and persists only a result equal to its
founding `vault_pk`.

**Encoding rule for every `‖` in this document**: each field
le32-length-prefixed, the whole tuple entry-counted — the republic-id
injectivity rule; never separators. Every such layout carries a
`molt-vault-*-v1` tag and a byte-pin test (§14).

## 6. The records

Everything the vault keeps is a handful of small records plus one opaque
file per deposit.

- **Vault key in the roster** (D11) — when the charter selects the vault,
  each seat's `vault_pk` is a fourth per-seat field of the sealed roster:
  the joiner derives it during the ritual and sends it with its join, the
  founder's table carries it, and every member checks it the way it
  checks `nostr_pk` (sign-what-you-see: its own value is its own
  derivation; every other one is a valid point and roster-unique). It
  needs a conditional roster tag (`molt-roster-v6` only when a vault key
  is present; a vault-less founding stays v5/v4 byte-identically) and
  every recompute site moves together. Pinned by construction: there is
  no announcement record, no vote, no seat without a key. Like `nostr_pk`
  it has no proof of possession — a seat naming a key it cannot open only
  loses its own shares, which it could withhold anyway.
  Every vault-capable joiner ALWAYS sends `vault_pk` (it cannot know the
  charter yet, which is chosen after every join); the founder drops the
  field from the table when the charter has no vault. The join MAC is
  unchanged: the member's sign-what-you-see check of its own `vault_pk`
  is the binding. Holders, readers and recovery read every `vault_pk`
  from the genesis roster or `founding_identities`, never from a working
  table, a Membership block or a frame; `verify_chain` rejects a
  membership or recovery change that would move a seat's `vault_pk`.
  **The roster rule is one-directional and split by door**, so live v5
  republics carrying the mock `vault` key keep verifying: everywhere, any
  `vault_pk` means tag v6, every seat keyed, `vault` in `features`,
  2 ≤ m ≤ n − 2, keys canonical and unique; only a NEW founding refuses
  `vault` without keys. **The real vault exists iff the genesis roster is
  v6**, never by `effective_features()`; the vault switch of a walk comes
  from the genesis (or anchor) of the chain being walked, never from node
  state.

  **Older builds are locked out (D14), fail-closed at every door:** a
  vault founding needs `vault_pk` in every join, so the founder refuses
  an older joiner (`needs a newer version`); an older build meeting a v6
  roster rejects the unknown tag; and the vault's chain variants stop an
  older reader (additive-only rule). The `vault` feature key already
  exists in older builds as a mock, so a v5 roster may carry it: the real
  vault exists only where the genesis roster is v6. A `vault` key in a v5
  feature set stays the mock forever.
- **Deposit** (gated proposal → chain block) —
  `{depositor, name, kind, m, holders, commitments[m], enc_share[n-1],
  payload: {hash, size}, nonce, sig_depositor}`; `nonce` is 16 random
  bytes, fresh for every deposit (§7).
  - `holders` = every seat except the depositor, in genesis founding-table
    order; a seat's Shamir x-coordinate is its 1-based position in that
    table (the depositor's position is simply unused), and `enc_share[]`
    follows the same order. After a cut the order is read from
    `founding_identities` (same order), never from a working or recovery
    table, and never re-sorted. `seat_i` in every AAD and info string is
    the holder's member name.
  - `sig_depositor` is the depositor's identity signature over the
    canonical record (`molt-vault-deposit-v1`, every field but the
    signature, `enc_share` and `nonce` included). Chain blocks carry no
    proposer, so without it any member could propose a record naming
    someone else as depositor — and, under D7, replace their deposit.
    Approvers, `verify_chain` and the fold all check it.
  - For n = 5, m = 3: four 80-byte HPKE shares (32 encapsulated key,
    32 share, 16 tag) plus three 32-byte commitments — well inside the
    proposal budget. The payload ciphertext is NOT in the record.
- **Grant** (gated proposal → chain block) — `{grant_id, secret_id,
  reader}`, `grant_id = SHA-256(molt-vault-grant-v1 ‖ secret_id ‖ reader ‖
  proposal_id)`. Content-derived, so it survives a cut that drops block
  heights; the answer AAD binds it (§8). Validity is a projection rule: a
  grant block whose version is not current at its height commits but is
  void.
- **A closed op set.** In a v6 chain an applied Vault payload is exactly
  `deposit` or `grant`; `vault_base` is legal only as the fold entry of a
  vault cut; anything else hard-rejects at ingest, at approve and in
  `verify_chain`. The generic `propose` on the vault is refused
  (`use vault_seal`).
- **Payload file** — the ciphertext of the text, at most 100 KiB + 40
  bytes, carried by the file plane (§9.2).

`secret_id` = SHA-256 over `molt-vault-secret-v1` ‖ republic id ‖
depositor ‖ name ‖ kind ‖ m ‖ holders ‖ commitments ‖ payload hash. It
covers the commitments and the payload but not `enc_share` — no
circularity with the share AAD below.

## 7. Deposit

Depositor-local and ephemeral until the proposal commits (chain rule):

1. **Refuse early** unless the vault is enabled (§3). Every holder's key
   is in the roster (§6), so every holder gets a share.
2. Draw a fresh `nonce` and deal deterministically:
   `deal_seed = HKDF(vault_ikm, "molt-vault-deal-v1" ‖ republic_id ‖ name ‖
   kind ‖ nonce)`; `s` = wide reduction of `HKDF(deal_seed,
   "molt-vault-secret-scalar-v1")`, the polynomial's coefficients from
   ChaCha20 seeded with `deal_seed`. The depositor stores nothing: it
   re-deals to answer a complaint and recovers `s` to re-seal. A reused
   nonce would reuse `s` across versions (whose `dek_info` is identical),
   so the fresh nonce is load-bearing.
   `DEK = HKDF-SHA256(s, info = "molt-vault-dek-v1" ‖
   republic_id ‖ depositor ‖ name ‖ kind)`.
   `payload_ct = XChaCha20-Poly1305(DEK, text, aad = "molt-vault-payload-v1"
   ‖ republic_id ‖ depositor ‖ name ‖ kind)` — not `secret_id`, which
   hashes the ciphertext and would be circular.
3. Feldman-split `s` among the holders at threshold m.
4. `enc_share_i = HPKE_seal(vault_pk_i, share_i, aad = "molt-vault-share-v1"
   ‖ secret_id ‖ seat_i)`, with the **ephemeral key derived**, not drawn:
   `ikmE_i = HKDF(vault_ikm, "molt-vault-eph-v1" ‖ secret_id ‖ seat_i)`.
   This is what makes a complaint decidable (below); the depositor stores
   nothing extra, it re-derives from its seed.
5. Publish the payload file on the file plane, then propose the deposit.

**Approval is a vote, gated by verification** (D12). The deposit is an
ordinary proposal the members decide on — name and kind visible (D8).
A holder's client offers `approve` only after (a) its share opens and
checks against the commitments, AND (b) it holds the complete payload
file and its hash matches. Without (b) a deposit could commit while only
the depositor holds the ciphertext, and die with them.

**The card counts what is proven, not what committed.** The block commits
under the ordinary chain rule (m approvals, the depositor's own possibly
among them). The card reads `committed` until **m holders other than the
depositor** have verified — `sealed` — and `hardened` once all n − 1 have.
Only `sealed` means the depositor's absence is survivable.

**Verification receipts and complaints** are not chain records. Each
holder publishes a `verified` (share AND payload, as (a) and (b)
above — `sealed` counts these) or `complaint` for a `secret_id` as a
control frame authenticated by its MLS sender credential (whose key IS
the identity key; `by != from` drops it), persisted at every member
(last-wins per holder) and re-sent on start — the mirror declaration
pattern (`mirroring.md`). Reveals and answers are authenticated the same
way. They are status, not consensus, and never enter the vault base.

**A complaint is decided, not just shown.** On a complaint from holder i
the depositor's client answers with `share_i` and `ikmE_i`. Anyone
recomputes `enc_share_i` from them and the pinned `vault_pk_i`:

- the recomputed ciphertext differs from the record → the answer is a
  lie; the depositor is named;
- it matches and `share_i` fails the Feldman check → the depositor dealt
  a bad share; the depositor is named;
- it matches and passes → the complaint was false; the complainer is
  named.

The reveal publishes one share, which lowers that deposit's effective
threshold to m − 1. **A re-seal is offered, never automatic** (D15): the
depositor's client offers it, and since a re-seal is a replace (D7) it is
a vote like any deposit (D12) — automatic re-seals would turn every false
complaint into a ballot. Until it commits, every member's card shows the
lowered threshold. The cost lands only on the version being replaced
(plus old backups, §4). A depositor who cannot answer (gone) leaves the
complaint open; the card stays at what is proven. A complainer who keeps
complaining against honest re-seals is named every time.

The UI says it in one line per fact, nothing more:

| where | text |
|---|---|
| card, bad share | `bad share from <depositor>` |
| card, false complaint | `false complaint by <holder>` |
| card, open | `complaint open` |
| card, after a reveal | `readable by <m-1> instead of <m>` |
| depositor's card | that line plus a `Re-seal` button |

Deciding complaints rests on the HPKE crate accepting a caller-supplied
ephemeral (RFC 9180 `DeriveKeyPair`): `hpke` 0.13 draws exactly one
private-key-sized `ikm` from the caller's RNG in `encap`, so a one-shot
RNG yielding `ikmE` gives the deterministic ephemeral (plan §1.2, pinned
by a test). Complaints are therefore decided; never a hand-rolled HPKE.

A denied proposal discards everything; holders who opened their shares to
verify learned one share each of a secret that never entered the vault.

## 8. Grant and read

1. **Grant:** any member proposes `VaultGrant {grant_id, secret_id,
   reader}`; it commits at m like every other change (D2: no delay). The
   reader is any seat; its key is in the roster.
2. **A grant binds one version.** It is valid only while its `secret_id`
   is the current version under `(depositor, name)` when it commits. A
   replace supersedes every pending grant on the old version (they drop
   the way a stale wiki patch drops at a re-base), and seats stop
   answering committed grants on a replaced version. Nobody can release a
   version the voters did not see, and a grant can never commit against
   content nobody holds any more.
3. **Unseal:** when a seat sees a committed grant — and again whenever the
   reader asks — it sends `resp_i = HPKE_seal(vault_pk_reader, share_i,
   aad = "molt-vault-resp-v1" ‖ republic_id ‖ grant_id ‖ seat_i)` over the
   group channel. The AAD binds the answer to this grant (and through
   `grant_id`, to this version and this reader). The answer frame carries
   no seat: the reader takes the seat, its x-coordinate and the AAD seat
   from the MLS sender. The reader's key is its FOUNDING `vault_pk`. A
   grant displaced by a reorg after it was answered is D17.
4. **Read:** the reader combines **any m valid shares** — its own if it
   holds one, plus answers — checking each against the commitments (a bad
   answer names its seat), derives the DEK, decrypts the payload file
   locally and shows the text. A reader who holds a share needs m − 1
   answers, so one dead seat never blocks a read; the depositor as reader
   holds none and needs m (and knows the content anyway).

**Answers are never stored, only re-requested.** A grant is permanent
chain state, so re-answering reveals nothing new. A reader who lost its
device, restored a backup from before the grant, or recovered with a
fresh MLS leaf (which cannot decrypt older group messages) simply asks
again. The plaintext is never persisted either.

Everyone else's client shows the grant as an audit entry — name, reader,
when — with the content locked.

## 9. Storage, cut, recovery, backup

The rule every vault byte obeys: **it can be rebuilt from the phrase, the
current chain, the surviving seats, and any one backup.** Nothing lives
only in the ephemeral log or only on one device.

### 9.1 The chain

Announcement, deposit and grant blocks are small (§6) and ride the
existing gated governance, additive-only.

### 9.2 The payload on the file plane

Each payload file is its own series on the file plane, with its own job
family beside the share family and the wiki base family
(`knowledge_base_scale.md` §4.9.7): a `kind` the resume routes, a
`PieceWanted` answer path keyed by the payload hash, a sink beside
`chain.state` (`vault/<secret_id>.bin`, sealed at rest). 100 KiB is three
44 000-byte pieces.

**Holding is mandatory, not mirror consent.** Every seat of a
vault-enabled republic holds every current payload — outside the mirror
on/off switch and outside the mirror quota. At 100 KiB per deposit it is
not a resource question, and a vault that only consenting seats hold
would make its availability a matter of preference settings.

### 9.3 The cut folds the vault like the wiki

Without a fold, every record would accumulate in the checkpoint blob —
the trust root a rejoiner is handed, which already strains a 65 408-byte
gift-wrap cap (`log_compaction.md` §B.6a). Enough deposits would make
recovery into the republic silently impossible.

So the cut **folds**, with every K6 rule carried over
(`knowledge_base_scale.md` §4.9.3–§4.9.6):

- Deposits are a last-write-wins slot keyed `(depositor, name)`, the
  depositor authenticated by `sig_depositor` — only the current version
  survives (D7). Grants on a dropped version go with it, audit included
  (D16).
- The blob carries ONE entry, `{"op": "vault_base", "hash", "size"}`: a
  commitment to the canonical bytes (`molt-vault-base-v1`) of all current
  deposits and grants (the keys live in the genesis roster). Receipts and complaints are not in
  it (§7).
- **A new fold variant, not only a tag: one variant, one tag.** Every cut
  in a republic whose genesis is roster-v6 is
  `ChainChange::CheckpointVault { upto, state_hash, wiki_folded }`, so an
  older build STOPS instead of reading a forgery; its state hashes under
  `molt-chain-checkpoint-v10` (the v9 layout plus `vault_pk` as a fourth
  field in BOTH identity tables). The vault group is always folded to one
  `vault_base` entry (an empty base when there was never a deposit); the
  wiki fold stays the proposer's choice and `wiki_folded` records it.
  Approval bytes: `molt-chain-change-v2`, discriminator 5, then the
  `wiki_folded` byte, `upto`, `state_hash`. A `Checkpoint` /
  `CheckpointFolded` in a v6-genesis chain is refused. Vault is one of
  the frozen `CHECKPOINT_V7_SURFACES`, so every existing cut keeps its
  bytes.
- **The fold takes the base held right now** (the §4.9.5 lesson):
  propose, co-sign, verify and apply pass the vault base this node holds
  at that moment, as a parameter — never one cached in the walk.
- **Once folded, always folded**: an unfolded vault cut after a folded
  anchor is refused.
- The vault base is a file-plane series like the wiki base. A node with a
  verified chain but not yet the base is **base-pending**: the vault
  answers a typed refusal with progress, never an empty list; it does not
  approve deposits or grants (it cannot verify them), queues its answers
  to committed grants until the base arrives, retires nothing against an
  empty base, and does not co-sign a cut.
- **The base is re-verified, never adopted on its hash alone.** Fold,
  catch-up and restore check the shape and `sig_depositor` of every
  deposit and recompute every `grant_id` under the base's own context; a
  failing base is deleted and refetched.
- **Retirement waits for the cut.** A replaced version's payload file is
  removed only once the replacing deposit falls below the anchor;
  mandatory holding covers every payload named by a deposit block above
  the anchor plus every current one.

### 9.4 Shares are derived, never stored

A seat never writes its decrypted share anywhere: it opens it from the
deposit record with its vault key whenever needed. Backups hold no
plaintext shares, and a recovered seat needs nothing but its phrase.

### 9.5 Recovery and backup

- **Recovery ritual** (phrase only): same identity, so the same vault key.
  The rejoiner adopts the chain, fetches the vault base and the payload
  files from any holder, and can answer and read like before.
- **Export and S3 backup** carry `vault_base.bin` and `vault/*.bin`,
  following the wiki-base pattern of 2026-10-05: the export ships a file
  only if it authenticates and names one it leaves out; the import plants
  only files that authenticate under the blob's key and drops the rest —
  a rotted vault file costs that file (re-fetchable while a holder lives),
  never the restore. The open checks the commitments. A republic in which
  every seat restores from backup after a cut keeps its vault.
- **Total loss** (phrase gone): that seat's shares are gone for good — the
  bound of §4.

### 9.6 Later: share refresh

Shares hang off seeds that never rotate. An attacker who collects m
phrases over the years can read everything deposited before. The
countermeasure is proactive refresh: every holder periodically adds a
share of a random zero-polynomial, after which old shares are worthless.
With n and m fixed for life it is unusually simple here. Not in v1 (D9);
recorded so the record format leaves room for a refresh epoch.

## 10. What a cheater can and cannot do

- **Forge someone's deposit or replace it** → `sig_depositor` fails;
  approvers, `verify_chain` and the fold refuse it.
- **Depositor hands out bad shares** → the holders complain; the reveal
  names the depositor; the card never reaches `sealed` on bad shares.
- **Member files false complaints** → the reveal names the complainer.
- **Depositor withholds the payload** → nobody can approve (§7 (b)); the
  deposit never commits.
- **Share holder answers with garbage** → the reader's commitment check
  names the seat; any m − 1 honest answers (plus an own share) suffice.
- **Coalition below m** → no grant commits, no answer is sent.
- **Re-routing an answer** → the AAD binds `grant_id`; an answer cannot be
  replayed for another grant or opened by another seat.
- **Release a version nobody voted on** → a grant binds one `secret_id`
  and dies with its version (§8.2).
- **m share holders out of band** → read everything (§4). Inherent.

## 11. Engine and MCP

- HPKE/Shamir work is CPU-only and small: seal, verify, combine run in
  command handlers; the single-owner actor stays as it is.
- Tools (co-equality): `vault_seal` (a new name deposits, an existing
  name replaces), `vault_reseal` (§7, D15), `vault_grant`, `vault_read`
  (reader side), and the list as `read_state(vault)` (`vault` object)
  with per-deposit `committed/sealed/hardened`, open complaints and
  grants. Approvals reuse `approve`. Answers, receipts,
  complaints and complaint reveals are INTERNAL.
- The seat token reaches `vault_read` like every other seat tool (D3);
  the read-only key never sees vault content.

## 12. What changed against rev 1

Rev 4 (the build plan's §1.3, 2026-10-05):
- **The vault key is salted with the founding `nostr_pk`**, not
  `republic_id` (§5), and `vault_ikm` is persisted, failing closed when
  missing.
- **Dealing is deterministic** from the depositor's seed and a fresh
  public `nonce` (§7); the depositor's phrase opens its own deposits
  (§4).
- **Control frames are authenticated by the MLS sender**; answers carry
  no seat (§7, §8).
- **One checkpoint variant and tag for every cut of a vault republic**
  (§9.3), the base re-verified, retirement at the cut, base-pending
  guarding every retirement and every co-sign.
- **The roster rule is one-directional** so v5 mock-vault republics keep
  verifying; the real vault exists iff the genesis is v6; the walk takes
  the vault switch from the chain it walks (§6).
- **A closed Vault op set** and grant validity as a projection rule (§6).
- **D17** (§2): displaced grants stay audited locally.
- **Libraries decided** (§13).

Rev 2/3 against rev 1:
- **No depositor protection, by decision** (D2, D3). Rev 1 implied a
  succession insurance; the honest product is a majority escrow.
- **Enablement bounds** (§3): 2 ≤ m ≤ n − 2 (rev 2 allowed n − 1, which
  tolerates no dead holder).
- **Founding only, keys in the roster** (D11). Rev 2's announcement
  records let one seat that never opened again block every deposit.
- **A deposit is a vote** (D12), not an automatic approval.
- **The depositor holds no share**, and `sealed` counts other holders
  only — the depositor's own share never helps the case the vault exists
  for.
- **Deposits are signed by the depositor**; chain blocks carry no
  proposer.
- **Payloads moved off the block** to the file plane. The proposal budget
  (`payload_fits`, roughly 95 KiB of plaintext at the default 128 KiB
  publish budget, less for larger rosters) cannot carry 100 KiB plus the
  record, and inline bundles would accumulate in the checkpoint blob.
- **The cut folds the vault** into a committed base, with every K6 rule.
- **Answers are re-requested, never stored**, and bind a content-derived
  `grant_id` instead of a block height a cut drops.
- **Grants bind a version**; a replace supersedes them.
- **Unseal quorum settled:** any m valid shares (rev 1's open question).
- **Approval also proves the payload is held.**
- **Complaints are decided** by a deterministic-ephemeral reveal.
- **The vault key binds `republic_id` and `identity_pk`.**
- **Feldman vs Pedersen, corrected.** Rev 1 rejected Pedersen because the
  reader could not verify answers — wrong: answers would carry
  `(s_i, t_i)` and verify the same way. Feldman stays (smaller records,
  one check against public commitments), with its computational secrecy
  stated (§4).
- Rejected and still rejected: threshold encryption with a DKG (n and m
  are fixed, which removes its one advantage; pairing crypto), plain
  Shamir without VSS (a lying depositor goes unnoticed), shares as plain
  MLS messages (the group channel is readable by all).

## 13. Library verdict

- **`vsss-rs` 5.1.0** — Shamir + Feldman over Ristretto, pure Rust.
  5.1.0, not 6.x: it rides the `curve25519-dalek` 4.1.3 / `rand_core` 0.6
  stack OpenMLS already pins; 6.x would duplicate the curve stack.
  Caller-chosen x-coordinates via `ParticipantIdGeneratorType::List`.
- **HPKE: `hpke` 0.13.0** (RFC 9180, pure Rust, same RustCrypto
  generation as the tree): its `encap` takes the caller's RNG, which makes
  the deterministic ephemeral possible (§7). `hpke-rs` (in tree via
  OpenMLS) is rejected: its only ephemeral override is a test feature
  that would unify into OpenMLS. Never hand-rolled X25519 + AEAD.
- `chacha20poly1305`, `x25519-dalek`, `hkdf`, `curve25519-dalek` — in the
  lockfile. The ring-free guard and the no-C posture hold.

## 14. Build phases (nothing started)

1. **V1 spike** — dep-lock `vsss-rs`, decide the HPKE crate (ephemeral
   control), red byte-pin tests for every layout: `molt-vault-secret-v1`,
   `-deposit-v1`, `-grant-v1`, `-share-v1`, `-payload-v1`, `-resp-v1`,
   `-base-v1`, `molt-roster-v6`, the vault-key and DEK info strings.
2. **V2 founding** — `vault_pk` through join, table, sign-what-you-see and
   `verify_sealed_roster`; conditional roster-v6; enablement bounds in the
   wizard, `verify_seal_proposal` and `set_features`. Keystones: a vault
   founding round-trips v6, a vault-less one stays byte-identical, a
   tampered or duplicate `vault_pk` is refused by the member.
3. **V3 deposit** — payload
   series with mandatory holding, signed deposit with verify-gated approve
   (share AND payload), receipts, complaints and their reveal, replace.
   Keystones: a deposit commits only once the approver holds the payload;
   a forged depositor is refused by approver and verifier; each of the
   three complaint outcomes names the right seat.
4. **V4 grant and read** — grant variant bound to a version,
   answer-on-commit and answer-on-request, reader reveal, audit list.
   Keystones: three nodes, the reader decrypts with one seat dead, a
   non-reader provably cannot, a reader restored from a pre-grant backup
   reads by re-requesting, a replace racing a grant supersedes it.
5. **V5 fold, recovery, backup** — the fold variant and conditional tag,
   the held-now rule, base-pending, export and import of the vault files.
   Keystones: deposit → cut → recover a seat → grant to that seat → it
   reads; every seat restored from backup after a cut → a grant still
   reads (the twin of `a_folded_wiki_survives_every_seat_restoring_from_backup`).
6. **V6 share refresh** (§9.6), after v1 ships.
