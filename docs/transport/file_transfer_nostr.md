# File transfer over the Nostr transport — the 445-chunk data plane

Status: **DESIGN — direction ratified, details open.** The user decided
2026-08-09: build the 445-chunk data plane (§10.7 lifts "OFF in V1").
**The open questions in §5 must be discussed before the build starts** —
this document is the discussion basis, not a finished spec.

## 1. What exists, and what the gate is

- The share/download surface is REAL and transport-agnostic: `ShareFile`
  posts a metadata-only chat message (path stays node-local, sha256 +
  size ride the message), `DownloadFile` fetches the bytes from the
  sharer, `RemoveFile` ends availability. Over LOOPBACK the bytes move on
  a dedicated queue pair (`molt-net/src/transfer.rs`: manifest + pieces,
  reorder-safe since 2ef58b0).
- Over relays the ENGINE refuses a share by name (`cmd_share_file`) and
  the GUI dims the button (§10.7 two-sided gate) — a relay republic has
  no queue pair, and nothing else carries bytes today.
- A chunker exists and is battle-tested (`molt-net/src/chunk.rs`):
  `msg id (16) | index u16 | count u16 | len u16` header inside the
  encrypted payload, bounds-checked reassembly, dedup by (id, index),
  bounded partials. The repo precedent for big payloads over relays is
  "445-level chunking, NOT Blossom" (`nostr_transport_marmot.md` §7,
  `mdk_evaluation.md`) — an external blob server (Marmot's media path)
  adds infrastructure, a second metadata surface and a clearnet
  dependency, against the self-host/onion posture (ADR-0004).

## 2. Shape of the design

- **The share message stays what it is** — metadata in the chat log,
  addressed by `MessageId`. No log-schema change for the bytes.
- **Bytes ride a parallel 445 event series, never the workspace log:**
  the file is chunked (`chunk_message`) and each chunk is sealed exactly
  like a 445 group frame (exporter-secret AEAD, same h-tag rotation), but
  published under the file's own deterministic msg id — receivers
  reassemble to a bounded disk cache keyed by the share's `MessageId`.
  The log stays state-sized; compaction (WP4a) never sees file bytes.
- **Store-and-forward within relay retention:** while the relays hold the
  chunk events, a downloader needs no live sharer. Past retention the
  existing `FileRequested` broadcast asks the SHARER to re-publish the
  series (same authenticate-by-MLS pattern as today) — availability
  honestly degrades to "sharer online", never silently.
- **The GUI un-dims** share/download on relay republics once the plane
  ships; every refusal keeps naming its reason (cap, aged out, sharer
  gone).

## 3. Budgets

One chunk must fit the per-event publish budget beside its header (the
`payload_fits` ceiling machinery is reusable). `count` is u16, so the hard
plane limit is 65535 chunks; the REAL cap must come from relay behaviour
(rate limits, Tor bandwidth, pool storage courtesy), not from the header.

## 4. Etappen

- **F1** — discuss §5, freeze the spec in this doc (status flip).
- **F2** — the chunk plane: publish/reassemble a sealed byte series over
  `MockRelay`, cap enforcement, disk cache with bounds; keystone tests
  red-first.
- **F3** — engine wiring: `cmd_share_file` over nostr publishes the
  series; `DownloadFile` reads cache-or-relay; GUI un-dim + progress.
- **F4** — the fallback: `FileRequested` re-publish past retention,
  RemoveFile semantics (sharer deletes → series never re-published),
  docs + status lines.

## 5. OPEN — decide before F2

1. **Size cap.** Proposal: 4 MiB default (≈70 chunks at ~60 KiB), config
   key for operators. Bigger files = out of scope V2?
2. **Publish timing.** Chunks published eagerly at share time (costs
   upload even if nobody downloads) or lazily on first `FileRequested`
   (first download waits for the sharer's upload)? Eager matches
   store-and-forward; lazy spares the pool.
3. **Cache bounds.** Per-workspace disk budget for reassembled files and
   fetched chunks; eviction order.
4. **Pool courtesy.** Rate-limit chunk publishes against the same hourly
   budget machinery the resends use, or a dedicated file budget?
5. **Retention honesty in the UI.** How the share card states "relay-held
   vs. sharer-only" without a wall of text (compact-text rule).
