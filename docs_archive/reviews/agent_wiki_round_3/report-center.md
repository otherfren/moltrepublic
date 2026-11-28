# Report Seat B (Mr. Center) - "Second Wiki Test", Round 3

(Verbatim the seat's final message, 2026-09-06 17:45 local.)

## 1. Holdings, proposals, collaboration

| Quantity | before | after |
| --- | --- | --- |
| Pages in the wiki | 125 | **212** |
| Pages with `sitz-b` | 56 | **81** |
| Chain head | 59 (checkpoint) | **126** (proposal 225) |
| `wiki_rev` | 0 | 19 (two checkpoints re-folded in between) |

**Own proposals: 28** (127–244), of which **27 applied**, 1 withdrawn-and-replaced (205 → 217). In addition, two dead leftovers from round 2 withdrawn at the start (25, 55 — duplicates of already applied patches).
**Others' proposals: 25 signed, 3 rejected** — 147 (two `authored_by` edges on a person page instead of on the work), 189 and 212 (reference to a file not yet persisted). All three were repaired by their authors within minutes and resubmitted (168, 189+192, 242); no rejection stayed a dispute.
**Rename: 1** — `standards/http-signatures.md` → `standards/draft-cavage-http-signatures.md`, all four incoming edges repaired (measured with `wiki_links direction:in`), alias "HTTP Signatures" moved to `rfc-9421.md`; afterwards `dangling 0`.
**Others' pages: 3 signed sections** — `standards/nip-29.md` (NIP-29 = MUC blueprint, 22 years later), `standards/nip-46.md` (signer vs. MSC3861: revocation is the difference), `protokolle/openpgp.md` (OpenPGP in XMPP tried twice, Experimental twice).
**Disputes: 2 new**, both with both pieces of evidence on the page and in the topic `disputes` — "Is MIX the successor of MUC?" (resolved as `competes_with`, not `supersedes`: Experimental 0.14.6 of 2020 against Stable 1.35.5 of 2026) and "What the AGPL switch of Synapse means" (resolved by decomposition: AGPL and CLA are two questions).
**Ontology version 5** (proposal 127): section 9, image and file grammar.
**Messages from my seat in this run: 124** — 14 group, 8 `sync`, 2 `ontologie`, 1 `disputes`, 99 in patch channels (including the engine's ⚖ system lines, which carry my name). Total in the run: 270 (Left 81, Right 65).

## 2. Images and files

Four PNGs drawn with PIL from queries of this wiki, one CSV as the data basis. All persistent, all referenced, `local.kind` is `own` everywhere at the end.

| File | Share ID | sha256 prefix | persist | Page | `files` block |
| --- | --- | --- | --- | --- | --- |
| B-protokollstapel.png (102817 B) | 6cb4e687… | `2db745a887c7` | 130 | uebersicht/matrix-xmpp-activitypub.md | `state: local` |
| B-ap-zustellweg.png (82295 B) | 2d525df0… | `4454feac3f71` | 133 | protokolle/activitypub.md | `state: local` |
| B-zeitachse-segment-v2.png (123482 B) | c060ffc1… | `1cebf89b01b5` | 220 | uebersicht/… | `state: local` |
| B-implementierungen-balken-v2.png (89062 B) | 2b0a7173… | `9b93ef09c733` | 223 | uebersicht/… | `state: local` |
| B-implementierungen-v2.csv (2312 B) | 7c797ce4… | `f446ed72b324` | 226 | uebersicht/… (as `[Datengrundlage](upload:…)`) | `state: local` |

**Times (own clock, the API provides none):** share 14:41:02/03 → all five `checksum` in `read_uploads` at 14:41:07 (< 5 s for 1.5 KB to 105 KB); persist proposals 14:41:1x; first block before 14:43:2x, all five applied before 14:49. The v2 round: share 15:18:59, persist applied before 15:22.

**Mirror experiment (poll every 20 s, the unit is the PIECE, not the byte):**

- `left-at-protocol-weg.png` 145 KB/4 pieces: persistent 14:56:14 → partial 14:57:36 → **mirrored 14:58:57** (2:43)
- `left-nostr-kind-baender.png` 104 KB/3: persistent 14:55:54 → **mirrored 14:57:36** (1:42)
- `mr-right-abloeseketten.png` 88 KB/3: persistent 14:49:06 → 1/3 14:49:26 → 2/3 14:49:46 → **mirrored 14:50:07** (1:01)
- `holders` grows from `[Sharer]` to all three seats exactly when the last piece lands. Cadence ~20 s per piece, matching `mirror_publish_interval_secs: 15`. I did not touch `set_mirror`.

**Prescribed experiments:** (a) Reference before persist, deliberately (proposal 145): `resolve_upload` read `temporary: true`, `wiki_edit` proposed **without any warning** — this is the reason for ontology 9. (b) 12-hex prefix pointing nowhere, `dry_run`: exactly one warning, `file reference unresolved: upload:0123456789ab`, and the call is refused. (c) `unpersist` verified: after the block the file reads `persistent: false` with a fresh `expires_ts`.

## 3. Facets and hygiene (final state)

`wiki_props`: 33 keys, `tags` n=977/207 distinct, `aliases` 790, `type` 199/9, `year` 131, `first_released` 96, `implements` 57, `authored_by` 56, `depends_on` 49, `notable_year` 45, `disputed_by` 21, `supersedes` 11.

`wiki_health` at the end: **dangling 0 · orphans 0 · key_drift 0 · files 0/0/0** at 212 documents. My hygiene round brought orphans from 15 to 7; the last orphan of my own I no longer linked in myself after the DIVERGENZ report, but handed out as a finished edit in the topic `sync` — Mr. Left took it over (proposal 263).

## 4. Sync history and the divergence

`HEAD h=59 last=0 docs=125 rev=0` (14:36) · `h=71 last=154 docs=135 rev=4` (14:52) · `h=83 last=178 docs=169 rev=14` (15:01) · `h=115 last=241 docs=199 rev=8` (15:24). Matching pair at 14:58/14:59 (Right and Left both `h=80 last=172 docs=159 rev=11`).

**First deviation:** Right's line 15:19:28 `h=96 last=194 docs=197 rev=26` against my `h=115`. He reported DIVERGENZ, I confirmed it with numbers: **my block 97 IS the compaction checkpoint and carries HIS signature next to mine** — his node co-signed the cut and then discarded the finished block; from 98 on all blocks are only `[Mr. Center, Mr. Left]`. Reaction: stopped filing proposals, kept signing, logged everything. **Both built-in detectors stayed silent throughout** (`read_chain.diverged = []`, `status.chain_diverged = []`) — at a distance of 19 blocks. It was noticed through three symptoms, none of which is a detector: a wave of `superseded` without a path conflict, ⚖ chat lines about decisions that the own node does not know, and a standing head while decisions keep running.

## 5. Friction log

`friction-center.md` (12.4 KB, marked with `[NEW]`/`[R2]`).

The heaviest NEW points: chain blocks carry **no timestamp**, seconds from share to persisted cannot be reconstructed from the API · a reference to a merely temporary file passes `wiki_edit` **silently** · `unpersist` requires a field `at` named in no description (guessed: the share `ts`) · a checkpoint folds the base and kills **every** open patch, and the proposer **loses its own signature** while the proposal still stands as `proposed` · replacing the bytes of a shared file on disk goes **unnoticed** by the node (it keeps reporting the old size and `available: true`) · `wiki_neighbors` answers during an index rebuild with an error that a `.get("docs", [])` books as an empty result — my first bar image would therefore have shown two zeros.

What ran without friction: `share_file` → checksum under 5 s, `approve/decline` with `note` (the reason always lands before the vote), `wiki_resolve` (prevented two duplicate pages: "MSC4186" and "Element X" already bound as aliases), `dry_run` + `with_patch` as a review tool, and the file grammar itself — `![alt](upload:<12 hex>)` resolved on every page at the first attempt.

## 6. Collaboration

Nobody blocked, overwrote or bypassed me. Left and I wrote the same insight (NIP-29 ≙ MUC) at the same time onto each other's page — no conflict, but almost a third copy; settled in the patch channel. My three rejections were without exception received on the merits and repaired. **The most uncomfortable finding concerns myself:** after I had rejected 147 for an edge in the wrong direction, I ran the same test over all 197 pages — **13 hits, all 13 on my own person pages from round 2**, none on the others'. Repaired in 214 and 238, disclosed in the topic `ontologie`. A direction mix-up is caught by no check of this wiki: neither `wiki_health` nor `wiki_edit` knows it, and when reading, the sentence sounds right.
