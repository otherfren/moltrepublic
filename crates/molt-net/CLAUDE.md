# Working in molt-net

Loaded when you work under `crates/molt-net/` — the transport layer. The
workspace-wide rules live in the root `CLAUDE.md`; this file holds only what
costs time to (re)discover in THIS crate.

## MLS / OpenMLS reference (`src/mls.rs`)

- Version pairing (they version independently): `openmls 0.8.1`,
  `openmls_traits 0.5.0`, `openmls_rust_crypto 0.5.1`,
  `openmls_basic_credential 0.5.0`, `tls_codec 0.4.2`. Ciphersuite
  `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` (matches our Ed25519 + X25519).
- `SignatureKeyPair::from_raw(ED25519, seed, pub)` wants the **32-byte Ed25519
  seed** (what `ed25519_dalek::SigningKey::to_bytes()` returns) — NOT a 64-byte
  expanded key, NOT seed‖pub.
- Persist the provider's storage by bincode-serializing its public byte-keyed
  `values` map — **not** JSON (JSON object keys can't be `Vec<u8>`). Reload with
  `MlsGroup::load(storage, &group_id)`; the signer round-trips via
  `SignatureKeyPair::read`.
- `MlsGroup::export_secret` takes the **crypto provider** (`provider.crypto()`),
  not the provider itself — the trait bound is `OpenMlsCrypto`.
- `RelayMessage`'s `Deserialize` is lifetime-unconstrained (owned `Cow`s), so
  `from_json` types as `'static` directly — no `into_owned` needed.
- `nostr::Timestamp::as_u64` is deprecated; use `as_secs`.

## Concurrent commits (N3 §1) — the rule that is easy to break

Two members can commit at the SAME epoch (two recoveries at once). Without a
shared rule they merge their own and diverge permanently under one epoch
number, silently. The convergence rule:

- `CommitKey(created_at, sha256(commit))` is the total order — **lowest wins**,
  timestamp first, digest last so it is not grindable.
- The stamp must come from the SAME source on both sides. There is no
  wall-clock default: `restore_member` REQUIRES `created_at`, and while the
  transport carries no per-event timestamp both sides pass `NO_CARRIER_STAMP`
  (the order then degrades to the digest — symmetric, just grindable). A
  local clock on the send side against a `0` on the receive side makes the own
  commit always lose, which is worse than not having the mechanism at all.
- The prior-state slot is armed on EVERY merge (committers AND bystanders), so
  it always points exactly one epoch back and every node can rewind onto the
  winner. Do NOT add a traffic-based expiry: it strands any bystander that
  accepted one message between two concurrent commits.
- The tiebreak is COMMIT-ONLY. An old-epoch application message must never
  reach it, or a rewind puts the group back into a state that can still read
  it — the hole `max_past_epochs = 0` exists to close.
- The rewind is transactional and disarms the slot first (the content type is
  cleartext framing, so a forged frame could otherwise roll a node back, or
  recurse until the stack blows).

## Test doubles worth reusing

- `MockRelay::run()` + `.url().await` — the in-process Nostr relay (dev-dep).
  `LocalRelay::new(RelayBuilder::default().nip42(...))` for auth-required
  relays.
- `tests/tor_probe.rs::forwarding_socks5` — a SOCKS5 proxy that negotiates
  userpass and really forwards, for anything that must prove a circuit rather
  than a lying proxy.
- `tests/common/mod.rs` — `proxy::Cuttable`, a cuttable TCP proxy (the only
  way to take a `MockRelay` down and bring it back on the same port; it can
  also go half-dead), and `fast_relay()` without the notes-per-minute
  limit. Three older near-copies of the proxy still live in
  `tests/nostr_window_roll.rs`, `tests/publish_pool.rs` and
  `molt-engine/tests/nostr_window_roll.rs` — fold them into `common` when
  you touch them.
- A "dead" port must be bound-then-dropped, never port 9: a host running the
  discard service would silently invert the test.
