# Delete Upload: a vote removes a temporary share for good

Status: EXECUTED 2026-09-08 - built in the change that carries this file
into the archive. Commissioned the same day ("temporary uploads should be
deletable too: a delete button right of the persist button, threshold
gated, then the file is gone for good"). Follows
`docs_archive/files/persistent_uploads.md` (persist/unpersist) and
`docs_archive/files/mirroring.md`.

## Goal

A "delete" button beside "persist" on every Temporary Uploads row. It is a
threshold vote on the Shared Files surface like the other two; once the
block seals, the share leaves both tables on every seat, nobody serves,
fetches or mirrors its bytes any more, and the sharer's own record that it
serves the file is dropped. Persistent shares carry no delete button: the
republic pinned them, so "unpersist first" (the existing rule for
`remove_file`/`delete_chat`) applies to the vote too.

## What exists

- `remove_file`: the SHARER alone withdraws its own temporary share (the
  message loses its file, `available: false`). No other seat can remove a
  share, and a sharer who left cannot be asked to.
- `persist {id}` / `unpersist {id, at}`: two Files ops, engine-written
  identity, `files_state()` fold (last op per id), one LWW checkpoint slot
  per share (`files.<id>`), `share_expiry_in` as the ONE expiry rule every
  consumer reads (tables, download gate, serve path, mirror sweep,
  compaction's share forgetting).

## D1 - The state model: a third op, terminal

```json
{"op": "delete", "id": "<32-hex message id>",
 "name": "...", "kind": "...", "size": 1234, "checksum": "<sha256>",
 "by": "<sharer>", "shared_ts": 1756900000, "key_b64": "...", "pieces": 3, "root": "..."}
```

- The proposer sends `{op: "delete", id}`; the engine fills the identity
  from the live share or its block (`prepare_files_proposal`), members
  ratify the concrete file (sign-what-you-see). No `at`: the block's seal
  is the moment, and a deleted share has no window to restart.
- Propose door: refused when the id is not a known share (live message or
  Files state), when the share is persistent ("unpersist first"), when it
  already left the tables (expired - nothing to delete), or while a
  persist or delete vote for it is open. An UNAVAILABLE share (sharer
  removed the file, or replaced it) CAN be deleted - cleanup is the point.
  Conversely a persist is refused while a delete vote is open, so the two
  can never race to a block whose approve door then refuses forever.
- Approve door (`check_files_vote`): the identity must match what this
  seat knows under that id; a persistent share is refused; availability
  is not required.
- Fold: `FileState::Deleted(meta)`, terminal; a later persist/unpersist
  cannot apply (both doors refuse: "deleted"). The LWW slot rule is
  unchanged (`applied_lww_slot` accepts `delete`): the cut keeps the
  latest op, and it must - the chat message may still be in the log, and
  a rejoiner must not see it come back as a plain temporary share.
- Effects, all derived from the fold (deterministic on every seat):
  - `uploads_view`: no row in either table.
  - `share_expired_in`: true for a deleted share, so the download gate,
    the serve path, the FileServed ingest gate, the mirror sweep and
    compaction's share forgetting all treat it as gone; the download gate
    answers `MoltError::FileDeleted` (a new variant - "deleted by vote" is
    not "aged out").
  - The chat read overlays `file.available = false` on the carrying
    message (`applied_values`, chat arm), so a chat bubble shows the file
    as unavailable on every frontend without touching the log.
  - `after_block_applied` on a Files delete: the sharer forgets its share
    path at once (`forget_share_path`); every mirror drops the job on the
    next planning beat (the `gone` filter reads the fold).
- What stays: the user's own source file on the sharer's disk (never
  touched, as with every other path), downloaded copies in members'
  exchange folders (their files), and pieces already published to relays
  until the relays expire them - the republic's engines refuse to serve,
  fetch or mirror them, which is what "gone" can mean over relays.

## D2 - The GUI

- Temporary table: a second vote column right of "Persist": header
  "Delete" / "Löschen", a 70px button "delete" / "löschen". A row with an
  open delete vote shows its `n/m` on the delete button; a row with EITHER
  vote open has both buttons disabled (no double proposals, no race).
  `UploadRow` grows `delete-vote`. The Persistent table is unchanged.
- The button issues `Command::Propose { surface: Files, payload: {op:
  "delete", id} }` through `cx.issue`, like persist; the proposal card
  title reads "Delete: <name> · <by>" / "Löschen: …".

## D3 - MCP

`propose` documents `delete {id}`; `read_uploads` and `download_file` say
a deleted share lists nowhere and downloads nowhere. No new `Command`.

## Keystones (red first)

1. core: `applied_lww_slot(Files, delete)` = the share's slot.
2. engine: the delete proposal carries the identity, is refused for a
   persistent share, an unknown id, and while a persist/delete vote is
   open (and persist while a delete is open); the applied vote empties the
   tables, the download gate answers FileDeleted, the chat row reads
   unavailable, the sharer's share path is gone, a persist afterwards is
   refused; an unavailable share is deletable; the approve door accepts
   the matching identity and refuses a persistent share; the mirror
   sweep's `gone` rule drops a deleted job.
3. ui: the delete button sits right of persist and fires `delete-upload`;
   an open delete vote marks the row and disables both buttons; the
   applied vote removes the row.
4. mcp: description coverage.

## The keystones, as built

- core, `molt-core/src/chain.rs`:
  `the_last_write_wins_slots_are_exactly_the_declared_ones` (the delete
  shares its share's slot).
- engine, `molt-engine/src/tests/files_tests.rs`:
  `a_delete_vote_removes_a_temporary_share_for_good`,
  `a_delete_is_refused_for_a_persistent_share_and_an_unknown_id`,
  `an_unavailable_share_can_still_be_deleted`,
  `a_delete_block_makes_the_sharer_forget_its_share_path`,
  `the_approve_door_takes_a_matching_delete_and_refuses_a_persistent_share`,
  `the_mirror_sweep_drops_a_deleted_job`.
- ui, `molt-ui/src/tests/gui/files.rs`:
  `a_delete_vote_marks_the_row_and_the_applied_vote_removes_it`,
  `the_delete_button_sits_right_of_persist_and_fires_its_callback`.
- mcp, `molt-mcp/src/lib.rs`:
  `every_tool_that_touches_a_file_reference_names_the_grammar`.

One trap worth keeping: a Slint element the table's clip cuts off is not
in the tested element tree at all - the delete column pushes the Temporary
table past 1200px, so the lookup test sizes its window wider.
