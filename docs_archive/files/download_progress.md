# Download progress: every file download says where it stands

**Status: PLAN, decided 2026-11-14 (§7: every recommendation taken, Q3
turned to yes) - in build.** Field report
(2026-11-14): a member could not download a temporary share; the
republic runs Tor + Nostr, so transfers take long and the operator sees
nothing but "0 %" or a bare warning mark.

Builds on `docs_archive/files/mirroring.md` (series v2, trickle sender,
resumable fetch job, mirror worker), `persistent_uploads.md` and
`docs_archive/transport/file_transfer_nostr.md`.

## 1. Goal

One status model for every download - a temporary share, a persistent
share, a mirror fetch - that both surfaces read the same way
(`read_uploads` and the Files tables), names the stage it waits on,
shows real progress, and fails only for a reason the operator can act on.

## 2. What exists (read 2026-11-14)

- `download_file` → `cmd_download_file` (`molt-engine/src/chat.rs`) picks
  one of: local mirror assembly, own-share copy, the relay plane
  (`nostr_download`), the legacy queue plane.
- Relay plane without a known series stamp: records `FileWanted`, parks
  the request in `files.pending` (memory only) and arms
  `WANT_TIMEOUT` = **90 s** (`transfer.rs`). No `FileServed` within 90 s
  → phase `failed`, reason "the sharer did not answer".
- A `FileServed` that arrives after the 90 s drains nothing (the park is
  gone); the stamp is cached, so only a manual retry starts the fetch.
- With a stamp: a persisted `FetchJob` (bitmap of verified pieces),
  resumed at open, no ceiling; `PieceWanted` after 10 min, then every
  30 min.
- Sharer: answers `FileWanted` at once with `FileServed`, queues the
  whole series on the trickle sender: one piece (44 000 B) per
  `mirror_publish_interval_secs` (15 s), only while the outbox is idle,
  the resend budget has headroom and the daily byte cap holds.
- `DownloadView { phase, percent, path, error }` on `UploadView`;
  phases `requested | transferring | done | failed`. GUI status cell:
  `0 %`, `n %`, `✓`, `⚠` (reason only in a toast). Wiki file refs:
  "Loading n %" / "Failed".
- Mirror fetches show `mirror_held / mirror_of` as a thin bar in the
  Persistent table; not tied to `download`.
- Logging: the requester side logs no download transition at info/warn.

Pace at the defaults: 44 000 B / 15 s ≈ 2.9 KB/s ≈ 10.6 MB/h - 1 MB
≈ 6 min, 10 MB ≈ 1 h, 100 MB ≈ 9.5 h, before any gate holds the trickle.

## 3. Likely causes of the field report

Unconfirmed until the logs of both seats exist (S0):

1. **The 90 s watchdog is shorter than a want/served round trip over
   Tor + relays.** `FileWanted` rides the group outbox to the sharer,
   `FileServed` back; each leg can sit in the delivery guarantee's
   backoff (10 s..600 s). The download fails while the sharer is already
   publishing.
2. **Sharer offline** (a temporary share is sharer-only until a series
   exists): the same 90 s failure, with a reason that does not say
   "offline".
3. **A late `FileServed` never resumes the parked download** (§2).
4. The trickle is held by a gate (outbox busy, budget, daily cap) - the
   download sits at 0 % with no sign why.

## 4. Design

### 4.1 Lifecycle without a deadline

Waiting is a state, not a failure - like the founding ritual (user
decision 2026-11-12: no deadlines, retry instead of abort).

| phase | meaning | ends by |
|---|---|---|
| `waiting` | `FileWanted` sent, no `FileServed` yet | `FileServed` → `fetching`; cancel |
| `fetching` | subscribed, pieces arriving or awaited | all pieces → `verifying`; cancel |
| `verifying` | whole file hashed against the share checksum | `done` / `failed` |
| `done` | landed | - |
| `failed` | a fault the operator must act on (§4.4) | retry |
| `cancelled` | the operator stopped it | retry |

- `WANT_TIMEOUT` goes. While `waiting`, `FileWanted` is re-sent every
  10 min (the sharer answers every want; a lost stamp is re-heard).
- The park is persisted: a `FetchJob` with `started_at: 0` = "no stamp
  yet" (additive), so a restart resumes `waiting` instead of dropping it.
- A `FileServed` for a parked id always starts the fetch.
- `requested` stays a valid phase string for older readers, only
  written for the instant between the command and the first transition.

### 4.2 The view (additive on `DownloadView`)

```
phase      waiting | fetching | verifying | done | failed | cancelled
percent    0..=100 (verified data pieces / count)
held, of   verified data pieces, data piece count
bytes      verified plaintext bytes; size = the share's byte length
since      unix secs the download started
last_piece unix secs of the last verified piece (0 = none)
rate       bytes/s over the last 10 pieces (0 until 3 pieces)
eta        secs, 0 = unknown (shown only once rate > 0)
wait       what it waits on: sharer | sharer_offline | relays | pieces | ""
asked      unix secs of the last FileWanted / PieceWanted
error      the honest reason (failed only)
code       stable error key (failed only, §4.4)
```

`wait` is derived on the actor: `waiting` + sharer offline in presence →
`sharer_offline`; `waiting` + online → `sharer`; `fetching` with no
piece for > 2 min → `relays` before the first `PieceWanted`, `pieces`
after.

Progress commands from the fetch task carry `held/of/bytes` (throttled
to one per second, as today); the actor computes `rate`, `eta`, `wait`.
`Event::FileTransfer` keeps its shape; `TransferPhase` gains `Waiting`,
`Verifying`, `Cancelled` and `Progress { percent, held, of }` (additive
fields with defaults).

### 4.3 The sharer's side

The sharer must stay online for a sharer-only share, so it sees what it
serves: `UploadView.serving: Option<ServeView { published, of, gate,
wanted_by }>` from its `PublishJob` cursor and the trickle's current
gate (`outbox | budget | daily | ""`). Same row, same table - "Serving
34 % - waiting: daily cap". The requester sees it too (§7 Q3): the server gossips
`ServeProgress { id, published, of, gate }` (`\x00molt-sprog-v1`) at
most once a minute per series while it publishes, and once at the end;
`DownloadView.sharer` carries the last one. Display only - a forged
frame misleads a bar, nothing else.

### 4.4 Errors: compact, one key, one remedy

| code | line (en / de) | remedy shown |
|---|---|---|
| `no_relay` | No relay reachable / Kein Relay erreichbar | Retry |
| `write` | Cannot write to <dir> / Kann nicht nach <dir> schreiben | Retry, folder |
| `checksum` | Checksum mismatch / Prüfsumme falsch | Retry |
| `changed` | The sharer's file changed / Datei des Teilenden geändert | - |
| `gone` | The share was removed / Freigabe entfernt | - |
| `expired` | The share expired / Freigabe abgelaufen | - |

`sharer offline` is no longer an error (`wait: sharer_offline`). An
expiry DURING a download: see §7 Q5. Each failure logs once at warn
with `id= code= reason=`.

### 4.5 Persistent shares

1. **Complete local mirror**: assembly as today; the assembly reports
   `verifying` then `done`.
2. **Mirror job running for the same share**: the download does not
   start a second fetch - it shows the mirror job's `held/of` and
   assembles on completion (one subscription, not two).
3. **No local pieces, sharer offline, other seats hold it whole**: today
   the want goes only to the sharer and the stamp gate accepts a
   `FileServed` only from the sharer, so the download cannot start.
   Proposal: an elected whole holder (same election as `PieceWanted`,
   `mirroring.md` §3.3) answers `FileWanted` by re-seeding its stored
   pieces (`PublishJob { stored: true }`) and announces `FileServed`;
   the requester accepts a stamp from the sharer OR a seat whose
   `MirrorStatus` says whole. Cost of a forged stamp: wasted waiting
   only - every piece verifies against the voted root. §7 Q2.

### 4.6 GUI

Temporary and Persistent tables, the status cell (same component in
both, and the wiki file-ref chip uses the same words):

- `waiting`: "Waiting for sharer" / "Sharer offline" (dim), a Cancel x.
- `fetching`: a bar `held/of`, label "34 % - 1.2/3.5 MB - ~12 min"; the
  ETA part only with a rate. Tooltip: pieces, rate, last piece ago, last
  ask ago, what it waits on. Cancel x.
- `verifying`: "Verifying".
- `done`: "✓ Saved" - click opens the folder (existing
  `open-folder` door if present, else just the tooltip with the path).
- `failed`: "⚠ <line>" in the bad colour, the full reason in the
  tooltip, a Retry button.
- `cancelled`: nothing but the Download button again.
- The sharer's own row: "Serving 34 %" plus the gate word.

Toasts on done/failed stay. All strings compact, no em dash, en+de.

### 4.7 MCP

- `read_uploads`: the `download` object carries the §4.2 fields, rows
  carry `serving` (§4.3).
- New tool `cancel_download { id }` (a human decision → a tool on both
  surfaces; the co-equality test lists it).
- `download_file` description: async, poll `read_uploads`; `failed` is
  retryable with the same call; `waiting` has no deadline.
- Read-only key: sees `download` and `serving` (no path - `path` is
  already the seat's business; keep the existing redaction rule).

### 4.8 Diagnostics (log, structured)

Requester: `download start id= size= pieces= via=relay|mirror|local`,
`download want id= n=`, `download served id= at= from=`,
`download progress id= held= of=` at info every 10 %, `download done
id= secs=`, `download failed id= code= reason=`, `download cancel id=`.
Sharer: `serve want id= from=`, `serve progress id= published= of=
gate=` every 10 %. Ids are message ids, never file names (a file name is
republic content).

## 5. Stages (each red first, each green on master)

- **S0 Diagnostics.** The §4.8 lines. Ships first (small) so the field
  case can be read; release with S1.
- **S1 Lifecycle.** No watchdog, re-want, persisted park, late
  `FileServed` resumes, cancel (engine + `cancel_download`). Keystones:
  a `FileServed` arriving 10 virtual minutes after the want still lands
  the file; a restart in `waiting` resumes; cancel removes job and
  `.part` and a retry works.
- **S2 View.** §4.2 fields, `wait` derivation, rate/ETA, `serving`.
  Keystones: phase/held/of across a two-node transfer over `MockRelay`;
  `wait = sharer_offline` with the sharer down.
- **S3 Persistent.** Join a running mirror job; holder answers
  `FileWanted` (if Q2 = yes). Keystone: persistent share, sharer
  offline, a whole mirror, a third seat downloads it.
- **S4 GUI.** Status cell, Cancel/Retry, tooltip, wiki chip, serving
  label. Headless tests per phase.
- **S5 MCP + docs.** Tool, descriptions, `mirroring.md` §3.2/§3.7
  amended; this doc to the archive.

## 6. Not in scope

Faster transfers (the pace is a privacy decision, `mirroring.md` §1);
a separate downloads page; pausing (cancel + resume from the kept
bitmap would be the same thing - see Q6).

## 7. Open questions

- **Q1** No deadline on `waiting` (re-want every 10 min until cancel) -
  or a long one (e.g. 7 days)? Recommendation: none, as in the ritual.
- **Q2** Whole mirrors answer `FileWanted` for persistent shares when
  the sharer is offline (relaxes the `FileServed` trust gate to whole
  holders)? Recommendation: yes - the root still verifies every piece.
- **Q3** Should the requester see the sharer's publish progress (a small
  control frame, at most once a minute)? **Decided: yes** - as a second
  thin bar in the same table cell, its meaning in the tooltip.
- **Q4** Show an ETA at all at ~3 KB/s with bursty gates?
  Recommendation: yes, prefixed "~", only after 3 pieces.
- **Q5** A temporary share expires while its download runs: finish
  (relays may already drop the pieces, NIP-40) or stop with `expired`?
  Recommendation: keep fetching what is there; fail `expired` only
  when the relays went quiet past the deadline.
- **Q6** Cancel keeps the `.part` and bitmap for a later retry, or
  wipes them? Recommendation: wipe - one clear meaning of cancel.
