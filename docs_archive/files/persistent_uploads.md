# Persistent Uploads: a vote pins a shared file for good

**Status: EXECUTED - D1-D4 and the two table fixes ship with the change
that carries this file into the archive (2026-09-03). The MIRRORING
addendum below was decided the same evening and is designed in
`docs/files/mirroring.md` (open, stages M1-M6). The shipping behaviour is
the code and its tests; D1-D3 below name the shipped identifiers.**
Commissioned on 2026-09-03 right after the Shared Files nav surface landed
(`Surface::Files`, core).

## Goal

Shared Files gets a second view, **Persistent Uploads**. A "persist"
button on a Temporary row moves the share there through the usual
threshold vote on the Shared Files surface; a persisted share never
expires. An "unpersist" button (also a vote) moves it back to Temporary
with a fresh expiry window. Both tables are otherwise identical.

Plus two table fixes the user asked for: the Type column (header and
cells) centred and scaled like the rest, and the checksum column replaced
by an info icon that opens a small modal with the full sha256 and a
copy-to-clipboard button.

## What exists

- A share IS a chat message (`chat_bus.md`, 2026-07-16): the uploads table
  is `State::uploads_view` over `chat_visible()`, the expiry is `ts +
  retention`, `download_file` and the sharer's serve path both refuse an
  aged-out share (`chat_ts_aged_out`), and WP4a compaction forgets
  `share_paths` / `prefs.shared_files` for a message that fell off the log
  (`log_compaction.md` C4). Nothing about a share is republic knowledge
  today; it is chat-ephemeral end to end.
- Governance: `Command::Propose { surface, payload }` → threshold →
  `ChainChange::Applied { surface, payload }` block → `self.chain.applied`
  projection per surface, read through `applied_values`. Organization uses
  `validate_org_payload` for op-specific checks and `applied_lww_slot` so a
  checkpoint keeps the last entry per setting; every other surface
  accumulates.
- `Surface::Files` is ungated as of this morning (`is_gated = false`,
  `read_state files` projects the uploads table). Both change here.

## D1 - The state model: two ops on the Files surface, metadata in the block

`Surface::Files` becomes **gated** (`is_gated = true`; `gated_enum`,
`propose`'s refusal and the nav's pending/accepted/declined views follow
from that). It stays core (never a feature key).

Two ops. `validate_files_payload` is the node-independent shape check at
BOTH doors (the wire ingest drops a malformed one, `prepare_files_proposal`
refuses it); `check_files_vote` at approve matches the identity against
this seat's own share and bounds the stamp:

```json
{"op": "persist",   "id": "<32-hex message id>",
 "name": "...", "kind": "...", "size": 1234, "checksum": "<sha256>",
 "by": "<sharer>", "shared_ts": 1756900000}
{"op": "unpersist", "id": "<32-hex message id>", "at": 1756900000,
 "name": "...", "kind": "...", "size": 1234, "checksum": "<sha256>",
 "by": "<sharer>", "shared_ts": 1756900000}
```

Both carry the identity: a checkpoint keeps only the LATEST op per share
(`applied_lww_slot`), so an unpersist must fold on its own.

- The ENGINE fills `name/kind/size/checksum/by/shared_ts` at propose time
  from the live share (the proposer sends `{op, id[, at]}`; a payload that
  already carries them is re-filled, never trusted). Sign-what-you-see:
  members ratify the concrete file identity, checksum included, and the
  block keeps that identity after the chat message is gone.
- `persist` is refused when the id is not a live (unexpired, available)
  share, or is already persistent, or has a pending persist. `unpersist`
  is refused when the id is not persistent or has a pending unpersist.
  `at` is the PROPOSER's clock (like every other member-supplied value):
  within `UNPERSIST_SKEW` (1 h) of local now and never before the share.
- Fold (`files_state()`): the Files applied log in order, last op per id
  wins → `HashMap<MessageId, FileState>`; `Persistent(meta)` or
  `Unpersisted(meta, at)`, `at` floored at the share's stamp.
  Deterministic from the log alone, so recovery, catch-up and a
  checkpoint-seeded holder rebuild it. A persisted share can neither be
  removed (`remove_file`) nor tombstoned (`delete_chat`) by its sharer -
  "persistent - unpersist first".
- Checkpoint: `applied_lww_slot(Files, payload) = Some("files.<id>")` -
  the cut keeps the latest op per file. This is a NEW slot family for a
  surface that has no checkpoint group yet, so every existing cut keeps
  its bytes (`checkpoint-v8` conditional group creation). The first Files
  block strands any seat on a build that predates `Surface::Files`
  (`charter_features.md` D1 rule) - the trade every new surface makes.

## D2 - The read contract

`uploads_view` becomes the union of two sources, deduplicated by id:

| source | temporary row | persistent row |
|---|---|---|
| chat-visible share, no Files state | yes, expiry `ts + retention` | - |
| Files state `Persistent` | - | yes, metadata from the block; `available`/`online`/`availability` from the live message when present, else `sharer-only`/offline-derived |
| Files state `Unpersisted { at }` | yes while `at + retention > now`, expiry `at + retention`, metadata from the block (the message may be gone) | - |

`UploadView` grows `persistent: bool` (`#[serde(default)]`). One
`Command::ReadUploads` keeps serving both tables (the GUI splits on the
flag; MCP `read_uploads` documents it); `read_state files` returns the
persist/unpersist applied log like every gated surface (the interim
uploads projection of this morning goes).

The expiry rule is ONE function, `share_expiry_in(&self, states, id) ->
Option<u64>` (None = never; `share_expired` is its clock check), and
every consumer reads it: `uploads_view`, `download_file`'s `FileExpired`
gate, the sharer's serve refusal (`net/files.rs`), the member upload
counts, and compaction's share forgetting (a persisted or not-yet-expired
unpersisted id keeps its `share_paths` / `prefs.shared_files` entry even
when its message is pruned). `share_identity(&self, id)` is the message
or the block (also when a tombstone took the file); `adopt_share_paths`,
the serve path and the `FileServed` ingest gate all read it.

## D3 - The GUI

- `Surface::Files.views()`: `uploads` "Temporary Uploads", `persistent`
  "Persistent Uploads", then `pending` / `accepted` / `declined` (hidden
  while empty, exactly the Organization rule - the Slint panes for those
  three get `s.key == "files"` alongside `"organization"`).
- One table component renders both views from `org-uploads` filtered by
  `persistent`; the Temporary variant shows `expires` + a "persist" button
  column to its right, the Persistent variant hides both and shows
  "unpersist" instead. A row with a pending vote shows the button
  disabled with the vote's `n/m` (no double proposals).
- The buttons issue `Command::Propose { surface: Files, payload: {op,
  id[, at]} }` through the same `cx.issue` path as the org edit modals; the
  proposal card title comes from `display_title` ("Persist: <name> · <by>"
  / "Unpersist: …", German "Dauerhaft: …" / "Befristen: …").
- Type column: header and cell both `horizontal-alignment: center`, both
  widths `60px * Theme.ui-scale` (the header/row alignment rule from the
  members table: fixed widths scale on BOTH sides).
- Checksum column → a 26px info button; click opens a `ConfirmModal`
  (file name, the full sha256 in monospace, "Copy" via the existing
  `copy-text` route, Close). `UploadRow` grows `checksum-full`, `vote`
  and `persistent`; the nav row also stays while a Files vote or its
  history exists (`files_row_visible`).

## D4 - MCP

`propose` documents the two ops; `read_uploads` documents `persistent`
and the expiry semantics; `select_view` lists `files: uploads/persistent/
pending/accepted/declined`. No new `Command` (co-equality untouched).

## Keystones (red first)

1. core: `Surface::Files` gated, views pinned, `applied_lww_slot(Files)`
   slot per id, checkpoint pin unchanged for a state without Files groups.
2. engine: persist fills metadata and is refused for an expired/unknown/
   already-persistent id; unpersist refused for a non-persistent id; the
   fold's last-op-wins; `uploads_view` union table (persisted share
   survives retention, unpersisted share expires at `at + retention`);
   `download_file` and the serve path honour the rule; compaction keeps a
   persisted share's path entry after its message is pruned.
3. ui: the two views and their buttons issue the right proposals; the
   pending-row disabled state; the checksum modal copies the full hash;
   header/cell widths of the Type column agree (headless geometry).
4. mcp: schema/description coverage via the derived enums.

## Addendum (user, 2026-09-03 evening): mirroring - OPEN

Persistent files are to be MIRRORED by every consenting member: an opt-out
switch above the Persistent table (default on), a quota (default 1 GB), a
private mirror folder (default next to the workspaces dir); switch and
quota plus each member's mirror status are shared with the republic (who
mirrors what); only metadata in the chain, the bytes on members' machines;
automatic, least-mirrored first, a warning when the quota is full;
torrent-style encrypted pieces pulled from other holders, never plaintext
on the wire; a "mirrored by N" column with a piece-progress bar while the
local copy is incomplete.

What exists: the 447 chunk plane (`file_transfer_nostr.md`): 44 KB
plaintext per event, exporter-secret AEAD (8-deep ring, epoch-bound), lazy
publish, 12 publish rounds per hour, 4 MiB cap; members reach each other
ONLY through relays (no inbound endpoint per node).

Forks put to the user (2026-09-03) and decided the same evening - the
answers and the design live in `mirroring.md` §1; the list stays as the
record of the question:

1. Data path: (a) relays as the piece store over the 447 plane, every
   holder re-publishes pruned pieces - MB scale, not GB; (b) direct
   member-to-member via an inbound onion service per node - a new
   transport capability; (c) the configured S3 media bucket as piece store
   - GB-capable, user-owned, one central point. Recommended (a) now, (b)
   as a later stage.
2. Target file size (the 1 GB quota does not fit the 4 MiB cap).
3. Keys: one random content key per file in the persist block (members
   only, MLS), pieces = AEAD(key, index); epoch-independent, byte-identical
   re-publishes, the mirror folder holds ciphertext only.
4. Manifest: the sharer computes the Merkle root over the piece hashes at
   SHARE time (32 bytes in the chat share), the hash list is fetchable as
   piece 0; the persist block carries the root; every mirror verifies each
   piece against it.
5. Scope: consent + quota per republic, the folder private in prefs.toml;
   the declaration (on/off, quota) rides the chain like the relay
   declaration (last-wins per member), the holder status (which pieces) is
   ephemeral gossip like presence - the "mirrored by N" column reads it.
6. Unpersist: mirrors keep the pieces until the new window ends, then
   delete.
7. Default folder: `<workspace_dir>/../mirror/<republic-id>/`.
8. Order: this core first (done), mirroring as its own stage.
