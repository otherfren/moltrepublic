---
status: accepted (implemented — `nostr` in the default build since N1)
---

# Nostr transport crypto: C libsecp256k1 via rust-nostr (a third transport-edge C exception)

**Context.** Nostr is fundamentally secp256k1/Schnorr (BIP-340 event signatures
+ NIP-44 ECDH) — the curve is forced by the protocol, only the implementation
is a choice. An N0 `cargo-tree` audit (2026-07-29) established that `rust-nostr`
(`nostr` 0.44) is hard-wired to the C `secp256k1`/`secp256k1-sys` crate:
non-optional (present even with `--no-default-features`), no `k256`/pure-Rust
backend feature, and no maintained pure-Rust nostr crate exists. So the
concept's earlier "pure-Rust via k256" assumption is false, and the real choice
is: accept the C library, or hand-roll NIP-01/44/59 + a relay client on the
pure-Rust `k256`.

**Decision.** Use `rust-nostr` with its C `libsecp256k1` backend. This is a
**third sanctioned transport-edge C exception**, alongside `ring` (default
build, SMP cert-pin) and `libsqlite3-sys` (opt-in embedded-Tor). It is
contained to `molt-net`; the roster and chain identity stay pure-Rust Ed25519
(`ed25519-dalek`). An optional migration to a pure-Rust nostr stack is a
follow-up, mirroring the open `ring`-removal follow-up.

**Why.** Don't-roll-your-own-crypto outranks pure-Rust purity for the NIP-44 v2
layer (constant-time ECDH, HMAC, padding) and Schnorr signing — `libsecp256k1`
is the gold-standard, extensively audited, constant-time reference, arguably
*safer* than the younger `k256` for side channels. The C toolchain is already
required (`ring`), so no new build prerequisite. The dependency touches only
transport envelopes, never roster/chain signing, so the crypto-trust surface
is bounded.

## Considered options

- **Hand-roll on pure-Rust k256 (rejected for V1):** keeps the pure-Rust
  posture, but means writing and auditing our own NIP-44 v2 / Schnorr / NIP-59
  code and a minimal relay client — more weeks and more bespoke-crypto risk.
  Kept as an optional later migration.
- **Take only rust-nostr's wire/relay types, avoid its crypto (not viable):**
  `secp256k1` is a non-optional dependency of `nostr`, so it is pulled in
  regardless of which modules we use.

## Consequences

- The pure-Rust posture (CLAUDE.md, "aspirational, two C exceptions") gains a
  third exception; update that note. Direction is still to *reduce* C where
  possible (the k256 follow-up).
- N1's nostr key derivation uses secp256k1 (rust-nostr key types), not k256.
- N0 must still confirm `tokio-tungstenite`+rustls does not re-introduce `ring`
  in a way that conflicts with the rustls-rustcrypto posture.

Full reasoning: `docs_archive/transport/nostr_transport_marmot.md` §8, and the N0 audit
recorded in memory (`nostr-transport-decision`).
