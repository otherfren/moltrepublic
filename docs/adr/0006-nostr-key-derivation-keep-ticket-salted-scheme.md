---
status: accepted
---

# Nostr key derivation: keep the N1 ticket-salted SHA-256 scheme, not NIP-06

**Context.** N1 derives the member's secp256k1 transport anchor from the
recovery phrase's entropy, salted with the seat's single-use invite ticket:
`SHA-256("molt-nostr-identity-v1\0" ‖ entropy ‖ 0 ‖ ticket ‖ 0 ‖ ctr)` with
rejection sampling for scalar validity (`molt-net/src/nostr.rs`). The MDK
evaluation (§7.8) noted that `nostr::nips::nip06::FromMnemonic` with a
passphrase is structurally the same idea — mnemonic + salt → key — but
standardized (BIP-39/BIP-32/NIP-06) and interoperable with Nostr wallets, and
required the choice to be recorded as an ADR.

**Decision.** Keep the bespoke N1 scheme. No migration to NIP-06.

**Why.**

- **No interop goal.** §10.3 decided "NIP-EE mechanics only, no Marmot
  interop"; a member's transport anchor is never used by an external wallet
  or client, so NIP-06 compatibility buys nothing here.
- **Our phrases are not BIP-39 mnemonics.** The recovery phrase is our own
  wordlist/format without a BIP-39 checksum; `FromMnemonic` would need a
  phrase→mnemonic mapping layer — new code, not less.
- **Landed and pinned.** The scheme shipped in N1, is byte-pinned by tests
  (including the entropy/ticket boundary anti-collision pin), and every
  roster-v3 anchor, MAC v2 and `republic_id` v2 commits to keys derived this
  way. Switching would re-key every identity for zero functional gain.
- **The salting is the point, and it is preserved either way.** The
  per-republic ticket salt (cross-republic unlinkability) is the actual
  requirement; the hash construction around it is not security-relevant
  beyond being a sound KDF, which a tagged SHA-256 with rejection sampling
  is.

**Consequences.** A future product goal of wallet-importable member keys
would reopen this (then via a phrase→BIP-39 bridge, a breaking anchor
change). Until then the `mdk_evaluation.md` §7.8 follow-up is closed.
Decided 2026-07-31 with the user; recorded in the concept §10.13.
