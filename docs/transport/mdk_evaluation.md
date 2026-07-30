# MDK evaluation — what to take from the Marmot reference implementation

Status: **EVALUATED 2026-07-31.** Subject:
[`marmot-protocol/mdk`](https://github.com/marmot-protocol/mdk) @ `1c51295`
(2026-07-30), workspace `0.9.10`, MIT, 21 crates, ~330k LOC.
Supersedes the one-line dismissal in `nostr_transport_marmot.md` §8.

> **Why this document exists.** The Nostr concept dismissed MDK in a single
> sentence — *"their account/storage/runtime overlaps our engine entirely"* —
> and nobody had read the code. That sentence is TRUE of `marmot-account`,
> `marmot-app`, `storage-sqlite` and `cgka-session`. It is **false of
> `transport-nostr-peeler`** (zero account model, zero storage, zero runtime)
> and mostly false of `transport-nostr-adapter`. We then hand-built layers MDK
> already had, and shipped two CRITICAL bugs doing it. See §6.

## 0. Verdict in one table

| MDK crate | LOC | Verdict |
|---|---|---|
| `transport-nostr-peeler` | 2.2k | **VENDOR** (adapted) — the envelope/validation layer |
| `cgka-conformance-simulator` | 10k | **BORROW the scenario catalogue** as our test design |
| `transport-nostr-adapter` | 5.1k | **PORT six specific behaviours**, not the crate |
| `cgka-traits` | 9k | **BORROW the seam shapes** (`TransportPeeler`, sync storage split) |
| `cgka-engine` | 28.5k | **REJECT** — identity-model conflict + forked OpenMLS |
| `storage-sqlite` | 28k | **REJECT** — C SQLite + C OpenSSL |
| `marmot-account` / `marmot-app` / `cli` / `uniffi` / `agent-*` / `quic-*` | 150k+ | **REJECT** — this is the part the original dismissal got right |

**Adopt by vendoring, never by `git` dependency.** See §3.

## 1. What MDK actually is

Not a library — a product workspace for the White Noise client, containing:
an OpenMLS-backed CGKA engine, a SQLCipher account store, a Nostr transport,
QUIC agent-stream previews, UniFFI mobile bindings, a CLI/daemon/TUI, Tamarin
formal models, and a conformance simulator.

Maturity signals: 842 commits, 18 authors (13 active in 90 days), 216 commits
in the last month, HEAD one day old, iOS/Android bindings released via CI,
`cgka-engine` has **more test LOC than source** (31.5k vs 28.5k), Tamarin
proofs run in CI (`--prove`, fails the build without a summary). No security
audit is claimed. Pre-1.0 with an explicit "`0.<minor>.0` is the breaking
line" policy.

**The published `mdk-*` crates on crates.io (0.7.x/0.8.0) are a DIFFERENT,
abandoned architecture.** Commit `9db9dbc0` (2026-07-02) replaced the entire
workspace (±336k lines) and set `publish = false`. Nothing in the current tree
has ever been published.

## 2. What we should take

### 2.1 `transport-nostr-peeler` — VENDOR (highest value)

The wire/crypto edge: no I/O, no account model, no storage, no async runtime
(tokio is a dev-dependency only), no `openmls` proper. Public surface is one
struct, one DTO and six constants.

What we would be buying — precisely the class of code that produced our two
CRITICAL findings:

- **NIP-01 id recomputation before trust** — `to_transport_message` verifies
  the event id against the recomputed hash *before* the id is used.
- **An exact-shape kind-445 tag validator** with a 13-case rejection table:
  duplicate `h`, unknown tag, empty tag, valueless `h`, `h` with an extra
  element, uppercase hex, short hex, non-hex, duplicate/negative/overflowing
  `expiration`. Strict single-tag extraction counts *occurrences*, never
  first-match.
- **Validate signature and shape BEFORE decrypting.**
- The NIP-59 welcome fail-closed chain, and bounded/validated relay tags on
  both wrap and peel.
- 34 tests that already encode all of it.

**Adaptations required** (this is a vendor, not a drop-in):

1. **Envelope divergence.** MDK implements current Marmot:
   `content = base64(nonce ‖ ChaCha20Poly1305(exporter_secret, plaintext,
   aad=""))` — one raw AEAD sealing, key = the exporter secret itself. Our
   concept §1 describes the older NIP-EE/EE.md shape (NIP-44 under a keypair
   *derived from* the exporter secret). These are different wire formats. We
   chose "NIP-EE mechanics only, no Marmot interop" (§10.3), so this is a
   **choice point that must be decided explicitly** — MDK's variant is
   simpler, 33 bytes smaller, and escapes the rust-nostr 65408-byte NIP-44
   send cap we pinned as a canary in N0. **DECIDED 2026-07-31 (concept
   §10.11): MDK's variant** — the peeler's envelope code and tests apply
   unmodified on this axis.
2. **Our h-tag rotation.** MDK's `h` is static per group. Our §4.4 requires
   deterministic 24h-window rotation — a deliberate metadata improvement over
   MDK. Vendoring unmodified would silently regress it.
3. **`wrap_welcome_with_metadata` requires a 32-byte KeyPackage event id** in
   the rumor's `e` tag. We dropped kind 443, so we have no such id.
4. Their rule is "exactly one `h`, at most one `expiration`, no other tag
   whatsoever" — a constraint on any future tag we might want.

### 2.2 Six adapter behaviours to PORT (not the crate)

Each is a bug we would otherwise ship and rediscover:

1. **`OutboundFanout`** — a durable, monotonic, restart-safe publish
   obligation (`NotAttempted → Attempting → Accepted/Failed` alongside an MLS
   `Pending → Confirmed/RolledBack`). Our delivery guarantee needs this shape.
   It also freezes the signed bytes, so a resend keeps its event id — without
   that, a fresh ephemeral key per wrap makes every resend a *new* event id
   and the rewind-resend double-delivers.
2. **NIP-59 inbox `since` widening** by the full 172 800 s timestamp tweak —
   without it, offline welcomes are permanently skipped.
3. **NIP-01 `duplicate:` on an `OK:false` counts as publication success** —
   this is what makes a rewind-resend safe.
4. **Multi-route-per-group with prior-route backfill** — retained historical
   routes get an unbounded backfill while the current route keeps its cursor.
   This is almost exactly our §4.4 rotation requirement, already tested.
5. **Canonical endpoint matching** (trailing slash / case / default port) —
   they hit this as a real routing bug.
6. **EOSE-based initial-sync gate** — "synced" means *every* endpoint sent
   EOSE.

Plus one **validator to adopt outright**: their `routing.rs` relay-URL check
rejects credentials (`userinfo`), fragments, and URLs over 512 bytes. Their
own dial chokepoint is missing it — we should have it at ours.

### 2.3 Test corpus and design — BORROW

`cgka-conformance-simulator` is 19 scenario vectors + property tests over a
25-verb chaos DSL (reorder, drop, duplicate, delay, partition, crash-restart,
concurrent commits). The **artifacts are not portable** — the harness is an
18-method in-process Rust trait with no IPC/CLI, expectations reach into
MDK-internal bookkeeping, and the vectors deliberately store no protocol bytes
("semantic behavioural traces, not byte-level KATs"). **The scenario design
is portable and valuable**: order-invariance under FIFO / reverse /
seeded-random delivery is a property test we can write against
`two_instances.rs` directly.

Also worth reading before N2/N3:
`docs/marmot-architecture/distributed-convergence.md` — its problem statement
(a returning client fetching a bag of messages from several relays, some from
abandoned branches) is our problem verbatim.

### 2.4 A real bug MDK exposed in OUR code

MDK has `CommitOrderingKey`: a content-derived total order over concurrent
commits (`source_epoch → priority → committer → SHA-256(bytes)`, digest last
because a digest-first order is grindable), plus snapshot-rollback fork
recovery.

**We have nothing.** `MlsMember::decrypt` merges any valid commit immediately,
and a losing same-epoch commit fails `process_message` and is swallowed by
`Err(_) => MlsDecode::Discard`. Two nodes that see commits in different orders
diverge **permanently and silently**.

Our exposure is narrow but real: commits happen at founding (founder only) and
at recovery, where `coordinator_rekey` runs on whichever node holds
`pending_recovery`. **Two concurrent recoveries for different members = two
concurrent commits = silent permanent fork.** This is days of work to fix, and
the red test should be written first. It gets worse, not better, on an
unordered multi-relay transport.

## 3. Why vendor and not depend

- **`publish = false` across all 21 crates** — verified against the crates.io
  sparse index (404 for every name). A `git` dependency would pin us to their
  entire workspace resolution and permanently block us from publishing.
- **The hard blocker: OpenMLS is a git fork.** MDK pins all five OpenMLS
  crates to `erskingardner/openmls@59e7d3b2` with `features =
  ["extensions-draft"]` (127 files / 13.6k lines diverged from crates.io
  0.8.1), plus `tls_codec ~0.5` against our 0.4.2. Cargo treats same-version
  different-source as two packages — the types do not unify. Scoping adoption
  to `transport-nostr-peeler`, which never touches `openmls` proper,
  **sidesteps this entirely**.
- **Churn**: 216 commits/month on a codebase wholesale replaced four weeks
  ago. Vendoring turns a breakage risk into a staleness choice.
- **Licence is the easy part**: MIT → GPL-3 is fine for dependency, vendoring
  and porting, attribution preserved. The reverse is not possible.

**Dependency posture is safe.** All candidate crates are `ring`-, `aws-lc`-,
`openssl`- and `sqlite`-free by default; their only C dependency is
`secp256k1-sys 0.10.1` via `nostr 0.44.6` — **byte-identical to what we
already sanction under ADR-0002**. The one trapdoor is the opt-in `sdk`
feature, which pulls `nostr-sdk → nostr-relay-pool → async-wsocket →
tokio-rustls → ring`. Our standing guard covers it — but it lives only as
prose in CLAUDE.md and **should become a CI gate**.

## 4. What MDK does NOT give us

Everything that makes this product what it is, plus several things we assumed
were free:

- **No Tor, no onion, anywhere.** `is_onion` appears zero times in `crates/`;
  no arti/tor deps. A `ws://…onion` endpoint is *rejected* as "plaintext
  public" — wrong for onion. Our entire ADR-0001/0004 posture is net-new. (And
  upstream `RelayUrl::is_onion()` is a naive `ends_with(".onion")` — no v3
  length, alphabet or checksum validation. Ours is stricter.)
- **No relay pool policy**: no empty-by-default, no per-session clearnet gate,
  no fail-closed-without-Tor. ADR-0004 has no counterpart.
- **No NIP-11 handling, no size budget, no chunking.**
- **No exporter-secret ring** (a peel miss is deferred to the engine).
- **No delivery guarantee** — their guarantee is "≥1 relay ACKed" plus relay
  retention. Our ACK-window/rewind-resend layer is unaffected and unhelped.
- **No threshold governance, chain, roster, or ritual.**
- **No sender-ratchet tuning** — they run the OpenMLS default `(5, 1000)`.
  We measured that failing in production (2026-07-28) and run
  `(5_000, 100_000)`. **On this axis we are ahead of them.**
- Their `max_past_epochs = 5` trades forward secrecy for delivery robustness;
  our `0` is deliberate so a recovery re-key really evicts a compromised
  device. Different threat models — ours is right for us.

## 5. One security gap worth knowing

In MDK, ephemeral kind-445 publishes and NIP-42 AUTH share the same
connection: `nostr-sdk`'s `automatic_authentication` defaults to true and MDK
never disables it, while `publish_prepared_event` sends on the same
authenticated pool. On any auth-gated relay the operator can link every
"anonymous" ephemeral-key event to the authenticated account — defeating the
purpose of per-event ephemeral keys. Our §7.5 already flags NIP-42 as a warned
leak; this makes it worse than assumed, and argues for a **separate publish
connection**.

## 6. The process lesson

We hand-built a URL host parser for the relay pool and shipped two CRITICAL
onion-spoofing bugs (a backslash and a `userinfo@` component both made a
clearnet host classify as `.onion` — auto-dialed, no warning). `url` 2.5.8,
the WHATWG parser every real client uses, was **already in our dependency
tree** via `nostr`, and MDK — the reference implementation for this exact
protocol — reaches for it everywhere and never hand-rolls a host parser.

Our own design doc argued *"a parser disagreement IS the bug"* and then
concluded "write a stricter hand-rolled parser" instead of the only conclusion
that follows: **use the same parser**.

Residual divergences from `url` still live in our allow-list parser (path
`..` collapsing, alternate IPv4 notations, no `is_local_addr` gate — we would
happily dial RFC1918). The follow-up is tracked in §7.

CLAUDE.md now requires a search-and-verdict step before hand-building any
non-trivial mechanism.

## 7. Follow-ups

1. **Rebuild the relay URL layer on `url`/`RelayUrl`** — parse with the real
   parser, take the host from `.domain()`, apply our v3-onion policy to the
   *parsed* host, keep `onion_classification_cannot_be_spoofed` as the
   regression net. `url` is pure Rust with no I/O, so it may live in
   `molt-core`; if its ICU tail is unwanted there, put the parsing in
   `molt-net` behind a `RelayLocator` newtype and keep the policy in
   `molt-core` reading the pre-parsed host.
2. **Add a local/private-address gate** (`is_local_addr` equivalent).
   **DECIDED 2026-07-31 (concept §10.14): gate like clearnet, don't
   hard-reject** — RFC1918/loopback/link-local/ULA go behind the ADR-0004
   acknowledgement + per-session activation (a LAN self-hosted relay stays
   possible, informed), never a silent dial.
3. **Fix the concurrent-commit fork** (§2.4) — red test first.
4. **Vendor the peeler** as the N3 envelope layer, with the four adaptations
   in §2.1. The envelope decision is made: current-Marmot raw AEAD (concept
   §10.11, 2026-07-31).
5. **Port the six adapter behaviours** (§2.2) into N5's runtime.
6. **Make the ring guard a CI gate**, not prose.
7. **Replace `socks5.rs` with `tokio-socks`** (already in the lockfile) and
   swap the hand-rolled constant-time MAC compare for `hmac::Mac::verify_slice`.
8. **Record the NIP-06 decision**: `nostr::nips::nip06::FromMnemonic` with a
   passphrase is exactly our ticket-salted derivation, with interoperability.
   If we keep the bespoke SHA-256 scheme, the ADR must say why.
   **DECIDED 2026-07-31: keep the N1 scheme — ADR-0006 records the why**
   (no interop goal per §10.3, landed + byte-pinned, our phrases are not
   checksummed BIP-39 mnemonics).
