# File transfer over the Nostr transport — the 445-chunk data plane

Status: **F1–F3 BUILT (2026-08-09/10); F4 partial.** The chunk plane
(kind 447, `molt-net/src/file_plane.rs`) and the engine wiring
(`FileWanted`/`FileServed` lazy round, share admitted on relays, GUI
un-dimmed) are on master — keystones
`molt-net/tests/file_plane.rs` and
`molt-engine/tests/file_over_relays.rs::a_shared_file_downloads_over_the_relay`.
Of F4, the re-publish loop ships (a stale stamp re-publishes on the next
`FileWanted`; a removed share stops serving via `available`), and the cap
is the `[storage] file_cap_bytes` config key (keep-live on wholesale
saves; default 4 MiB). STILL OPEN: the share card's one-word
relay-held/sharer-only status (§5.5).

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
- **Lazy publish, then store-and-forward:** the share itself moves no
  bytes. The FIRST `FileRequested` makes the sharer publish the chunk
  series (so the first download needs the sharer online and waits out
  the upload); from then on the relays hold the events and every further
  download needs no live sharer — until relay retention prunes them, at
  which point the next `FileRequested` triggers a re-publish. The same
  authenticate-by-MLS request pattern as today; availability degrades
  honestly to "sharer online", never silently.
- **The GUI un-dims** share/download on relay republics once the plane
  ships; every refusal keeps naming its reason (cap, aged out, sharer
  gone).

## 3. Budgets

One chunk must fit the per-event publish budget beside its header (the
`payload_fits` ceiling machinery is reusable). `count` is u16, so the hard
plane limit is 65535 chunks; the REAL cap must come from relay behaviour
(rate limits, Tor bandwidth, pool storage courtesy), not from the header.

## 4. Etappen

- **F1** — ✅ spec frozen (this document; decisions in §5).
- **F2** — ✅ the chunk plane (kind 447, sized chunker, cap + checksum
  refusals, honest miss) — `molt-net/src/file_plane.rs`, keystones in
  `molt-net/tests/file_plane.rs`.
- **F3** — ✅ engine wiring: the share is admitted on relays (metadata
  only), `DownloadFile` fetches a known series or parks on a `FileWanted`
  round; the sharer publishes lazily and announces via `FileServed`; the
  GUI share button un-dimmed. Keystone `file_over_relays.rs`. (No disk
  chunk cache in this cut: a fetch streams straight to the download dir —
  the cache bound of §5.3 becomes relevant only with previews/re-serves.)
- **F4** — partial: the stale-stamp re-publish loop, the RemoveFile stop
  (`available` gates serving) and the `file_cap_bytes` config key (§5.1)
  ship. OPEN: the share card's one-word availability status (§5.5).

## 5. Decisions (user-ratified 2026-08-09)

1. **Size cap: 4 MiB** (≈70 chunks at ~60 KiB), as a config key so
   operators can raise it deliberately. Bigger files are out of scope
   until the plane has field mileage.
2. **Publish timing: LAZY.** The first `FileRequested` triggers the
   sharer's chunk upload; nothing is published for a share nobody
   downloads. The first downloader waits out the upload — the share card
   says so (compact).
3. **Cache bounds (default, not user-asked):** 256 MiB per workspace,
   LRU eviction of fetched series; reassembled downloads land in the
   session download directory and leave the cache accounting.
4. **Pool courtesy (default):** chunk publishes ride the same hourly
   publish budget machinery as resends — no second budget to reason
   about; a spent budget holds the upload and says so.
5. **Retention honesty (default):** one status word on the share card —
   relay-held / sharer-only / gone — mapped from the last fetch outcome.
