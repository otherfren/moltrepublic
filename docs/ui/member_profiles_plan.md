# Member profiles: picture + description, vote-gated, with poke everywhere

**Status: PLAN (decided 2026-08-18) — not built; all forks resolved with the user, ready to execute**

## Goal

Organization → Members rows grow a square profile picture (placeholder +
edit button) and a description column. Both are edited only by the member
they belong to, and every edit is a real gated proposal on the Organization
surface — same flow as the existing org logo (`set_image`): modal → file
pick → `Command::Propose` → threshold vote → `Applied` chain block → fold.
Additionally, every GUI site that renders a username offers "Poke member"
on right-click, reusing the two poke menu patterns that landed 2026-08-18
(commits 96c4a7d, 46d312e).

No new `Command` is needed: the edits ride `Propose`/`Approve`/`Decline`/
`Withdraw`, the reads ride `ReadMembers` — co-equality untouched
(`co_equality_every_command_is_a_tool_or_documented_internal`,
`crates/molt-mcp/src/lib.rs:1614`, stays green without changes).

## DECISIONS (user, 2026-08-18)

| # | Decision |
|---|----------|
| a | **GUI auto-fits the image** to the engine-served budget (center-crop square, stepwise downscale). The derived cap stays the authority; the user is told "up to ~70 KB, roster-dependent", never a fixed 100 KB. |
| b | **One proposal payload** carries the bytes (sign-what-you-see); no side channel. |
| c | **Placeholder = `users.svg`**, colorized accent in an accent-soft box (org-logo visual language). |
| d | **Description cap 500 chars**, engine-enforced, counter in the edit modal. |
| e | **Two separate ops** (`set_member_image`, `set_member_desc`) = two independent votes. |
| f | **80px row, description on two lines** (user chose this over the 64px single-line variant). Consequence, binding for §5: the presence dot and the avatar are **top-aligned to the name line**, not vertically centered - `y: (name-line-height - self.height) / 2` inside a top-packed VerticalLayout, never the row-centered `y: (parent.height - self.height) / 2` that 40px rows use. The description Text gets `wrap: word-wrap`, `max-lines` behaviour via a fixed 2-line height box with `overflow: elide`, and the full text stays available as a HintTip on hover. |
| g | **Declare LWW slots** for the new ops (mixed-build checkpoint divergence accepted, same precedent as set_relays / set_features). |
| h | **Square is engine-enforced** (`w != h` refused with the compact "the image is 800x600 - it must be square"); org `set_image` stays unconstrained. |

The reasoning behind each, as explored:

**(a) The requested "under 100 KB" does not fit one publish.** The real cap
is transport-derived, not chosen: `image_headroom` (engine
`crates/molt-engine/src/proposals.rs:181`) measures **70 KiB decoded image
bytes at a 3-seat roster** (verified by running
`the_derived_ceiling_replaces_the_chosen_one`, proposals.rs:2020), and it
shrinks as the roster grows (every seat's attestation rides the block —
`a_larger_roster_leaves_less_room`, proposals.rs:2040; roughly ~0.1 KiB per
extra seat). 100 KB would need a second mechanism; ~64–70 KiB is what one
proposal carries. Recommendation: keep the derived cap (no second constant —
that mistake was already fixed once, proposals.rs:2016 comment) and let the
GUI downscale to fit (see (b)); state "up to ~70 KB, roster-dependent" to
the user instead of 100 KB.

**(b) Image rides ONE proposal payload (recommended) vs a side channel.**
One payload = the org-logo precedent: sign-what-you-see (members vote on the
actual bytes, proposals.rs:351–354), no transfer, no availability problem.
A side channel (file-transfer/chunking) would carry bigger images but breaks
sign-what-you-see and invents a mechanism for a 48-px avatar. Recommendation:
one payload, with the GUI auto-fitting before proposing: center-crop square,
downscale (start 512², shrink stepwise until the encoded size fits the
engine-served budget), re-encode PNG/JPEG — the `image` crate is already a
dependency of both molt-ui and molt-engine (`crates/molt-ui/Cargo.toml:36`).
The engine stays the authority (`validate_payload_fits`); to give the GUI an
honest target, add `image_budget: u64` (the current roster's
`image_headroom`) to the `Status` reply — additive `#[serde(default)]`.

**(c) Placeholder look: icon (recommended) vs initials.** The org logo
placeholder is `assets/republic.svg` colorized `Theme.accent` inside an
accent-soft rounded box (`crates/molt-ui-window/ui/app.slint:5207-5214`).
Mirroring it keeps one visual language; `assets/users.svg` already exists.
Initials (the collapsed-brand idiom, app.slint:2078) look more personal but
add a per-name code path for one glyph. Recommendation: users.svg, colorized,
accent-soft box.

**(d) Description length cap.** The payload gate alone would allow ~70 000
chars — a table column is not a charter. Recommendation: engine-enforced
**500 chars** (`DESC_MAX` in proposals.rs, refusal names the number), GUI
counter in the edit modal. Single-line elided render with the full text via
`HintTip` on hover.

**(e) One profile op or two: two (recommended).** Two ops
(`set_member_image`, `set_member_desc`) mean two independent votes, two LWW
slots, and simple cards (image card reuses the set_image preview, desc card
the generic Ist/Soll pair). One combined op would re-ship the image bytes on
every description tweak and force a combined card. Cost of two: a member
updating both fields triggers two votes — accepted, matches the org
precedent (name/charter/logo are separate votes too).

**(f) Row height / layout — DECIDED: 80px, two-line description.** Today
40px (app.slint:5672). The chosen shape: 56px avatar column ahead of the
name, description as a stretch-2 column between name and the id fingerprint,
wrapping over two lines with elide + HintTip for anything longer. More text
readable without hover; the table gets taller, and the presence-dot
alignment idiom must change (see §5 — top-align to the name line, never
row-center). Sorting: the new columns are NOT sortable (description sorting
is noise); the existing sort columns stay.

**(g) Mixed-build checkpoint divergence (accept, but decide knowingly).**
Declaring LWW slots for the new ops changes how a checkpoint SUMMARIZES a
chain that contains them. An older build treats unknown ops as accumulating
(the conservative rule, `crates/molt-core/src/chain.rs:411-414`), so at
`propose_checkpoint` time old and new builds recompute different state
hashes (`own_checkpoint_state` compare, `crates/molt-engine/src/chain.rs:
2936-2937`) and the checkpoint fails threshold on a mixed-build republic
once a profile op applied. This is NOT a byte-layout change (no tag bump —
see "Checkpoint ripple") and it is the exact precedent of `set_relays` /
`set_features` when they gained slots. Alternative: leave the ops
accumulating (no divergence) — but then every superseded ~64 KiB avatar
survives every checkpoint forever, which defeats compaction. Recommendation:
declare the slots.

**(h) Square enforcement: engine-hard (recommended) or GUI-only.** The GUI
auto-crops (b), but MCP agents propose too — co-equality says the contract
lives in the engine. Recommendation: engine refuses non-square
`set_member_image` (`w != h`) with a compact message ("the image is 800x600
- it must be square"); the dimension read already exists
(`image_decodable`'s `into_dimensions`, proposals.rs:276-280). The org
`set_image` stays unconstrained (no retro-rule).

## Explored facts (file:line)

### The org-image flow end to end (the precedent to clone)

- **Modal**: `ol-dlg` ConfirmModal, app.slint:7978-8070 — current image +
  path, draft field, pick button, `alt-label` = "remove" only when an image
  is set (7984). Opened by the status-card pencil (app.slint:5224-5236).
- **Pick**: `on_org_logo_pick`, `crates/molt-ui/src/lib.rs:1705-1723` — rfd
  `AsyncFileDialog`, filter `png/jpg/jpeg/webp/gif/bmp` (no svg — L1), only
  the path lands in the draft.
- **Propose**: `on_org_propose`, molt-ui:1588-1691 — `set_image` reads the
  file off the UI thread, pre-checks with the real preview decoder
  (`image_from_bytes`, molt-ui:2115), embeds `bytes_b64`, sends
  `Command::Propose { surface: Organization, payload }`; the "Proposed"
  toast rides the OUTCOME (1649-1669).
- **Engine gates (local propose)**: `validate_org_payload`
  (proposals.rs:336-390; set_image arm 355-361 — bytes required +
  `image_decodable`), `image_decodable` (256-287 — magic-byte sniff, header
  dims, ≤8192², SVG refused), `validate_payload_fits` (198-224 — refusal
  names the KiB that would fit).
- **Engine gates (wire ingest)**: the `WorkspaceEvent::Proposed` arm,
  `crates/molt-engine/src/net.rs:1385-1424` — numbered node-independent
  drops: (1) `payload_fits`, (2) set_image decodable, (3) set_relays
  non-empty. The self-edit gate becomes drop (4) here.
- **Payload cap derivation**: `transport_plaintext_ceiling` =
  `max_plaintext_for(DEFAULT_SIZE_BUDGET)` (proposals.rs:127-129);
  `DEFAULT_SIZE_BUDGET = 128 * 1024` (`crates/molt-net/src/relay_runtime.rs:71`);
  `max_plaintext_for` (`crates/molt-net/src/envelope.rs:142-149`);
  `applied_block_plaintext_len` prices the WORST-case block — n attestations
  (proposals.rs:136-167). Measured headroom: **70 KiB at n=3** (see (a)).
- **Applied read**: `org_effective` fold (proposals.rs:1270-1331);
  set_image arm 1299-1317 — the reference is the materialized
  `logo.<ext>` path when a storage dir exists, else the display value.
- **File materialization**: `sync_logo_file` (`crates/molt-engine/src/
  session.rs:1439-1462`, called from `after_org_applied` 1421-1431 and at
  open) → `StorageHandle::set_logo` (`crates/molt-storage/src/lib.rs:2463`,
  writer impl 1386-1401: drop stale `logo.*`, idempotent byte compare).
- **GUI applies**: molt-ui:5437-5445 — reload only when the path changes,
  decode by CONTENT.
- **Card**: `ProposalRowData` (molt-ui:4632-4678, `image_op`/`img_b64`),
  built at molt-ui:5895-5901 (`image_op: matches!(op, "set_image" |
  "remove_image")`, `img_b64` straight from the payload); inline preview
  `on_view_proposal_image` (molt-ui:1766-1789), save (1792). Slint struct
  `ProposalRow` at `crates/molt-ui-window/ui/theme.slint:114-147`.
- **Labels**: engine `decision_summary` (proposals.rs:966-1003 — image
  bytes NEVER in a chat line), `change_summary` (304-330); UI
  `org_op_label` (molt-ui:6696-6713) + `display_title` (6720-6748) with the
  member-targeted precedent `restore_member` → "Restore seat: {member}"
  (6724-6732).

### Applied projection + checkpoints (the LWW slot mechanism)

- `applied_lww_slot(surface, payload) -> Option<&'static str>`,
  `crates/molt-core/src/chain.rs:419-433`: set_image|remove_image share ONE
  slot `"organization.image"`; undeclared ops accumulate (conservative).
- Consumer: the checkpoint summary retain, engine chain.rs:680-681. Tests:
  molt-core chain.rs:887-912.
- `checkpoint_canonical_bytes` (molt-core chain.rs:455+, tag
  `molt-chain-checkpoint-v7`/v6 conditional): the SLOT RULE decides which
  applied entries survive the cut; the byte LAYOUT of the serialized state
  is unchanged by new slots → **no tag bump**. But verification recomputes
  the summary from each member's own chain (engine chain.rs:2936-2937) →
  open question (g).

### Members table today

- Engine `members_view` (proposals.rs:1675-1735) → `MemberView`
  (`crates/molt-core/src/lib.rs:5131-5157`; additive `#[serde(default)]`
  fields are the established evolution rule).
- MCP `read_members` (`crates/molt-mcp/src/lib.rs:848-853`) — description
  must gain the two profile fields.
- GUI: `MemberRow` struct (theme.slint:157-165); the table rows
  app.slint:5671-5810 (height 40px at 5672; name/split, id+copy, pk+copy,
  last, uploads, recovery column); molt-ui row mapping 5367-5380 with
  in-place `sync_rows` (molt-ui:2050 — never swap the ModelRc wholesale).

### The two poke menu patterns (landed today)

- `Command::Poke { member }` (molt-core:4663), MCP tool `poke`,
  GUI callback `root.poke(string)` (app.slint:926) — all exist; reuse only.
- **Pattern A — plain `ContextMenuArea` with `enabled:` gating** (no opaque
  TouchArea over the region): Members row, app.slint:5686-5695 —
  `enabled: root.cfg-poke-enabled && m.name != root.node-member`.
- **Pattern B — explicit `show()` dance** (an opaque TouchArea eats the
  right-click): `MemberPill`, `crates/molt-ui-window/ui/parts.slint:893-943`
  — `pointer-event` down+right → `mpmenu.show({x: mouse-x, y: mouse-y})`;
  `pokable` prop threaded in at app.slint:4577-4582.

### Username render sites — the poke rollout inventory

| # | Site | Where | Pattern | Note |
|---|------|-------|---------|------|
| 1 | Members table row | app.slint:5671 | done (A) | shipped 46d312e |
| 2 | Presence-strip MemberPill | parts.slint:882-945 | done (B) | shipped 46d312e |
| 3 | Chat author header (`line.lead`, `line.first` only) | parts.slint:536-551 | **A** | no TouchArea covers the header layout (verified: ChatRow's TouchAreas are the quote teaser, body TextInput, and right-edge action buttons, parts.slint:786-830); wrap the header HorizontalLayout; gate `pokable && !line.own && !line.system` — thread a `pokable` prop + `poke()` callback through ChatRow like MemberPill |
| 4 | Read-receipt dots (`rc.name` in HintTip) | parts.slint:695-720 | **B** | `dot-ta` TouchArea exists for the tooltip → `pointer-event` + show() |
| 5 | Proposal-card vote pills (`v.member`) | parts.slint:1648-1687 | **A** | the pill Rectangle has no TouchArea; per-pill menu; also covers the decision-chat header card (same component) |
| 6 | Decided-votes table dots (`v.member` in HintTip) | parts.slint:1235-1260 | **B** | `vd-ta` TouchArea exists |
| 7 | Uploads table sharer cell (`u.user`) | app.slint:6037-6055 | **A** | row has no covering TouchArea (download is a per-cell button) |
| 8 | Chat tombstone "deleted by X" | parts.slint:621-622 | **A** | wrap the one Text |
| 9 | Card "declined by X" label | parts.slint:1410-1426 | **A** | wrap the one Text |
| — | Chain-history signers | theme.slint:65 | skip | engine pre-joins "petra, walter" into ONE string — no per-name target without an engine change; low value |
| — | Quote teaser "author: body" | parts.slint:513-519 | skip | combined teaser string; click already jumps to the original |
| — | Wizard/ritual seats, invite previews | — | skip | pre-founding: no open workspace, `Poke` refuses |

Gating everywhere is the shipped rule verbatim: `cfg-poke-enabled` and never
the own seat. ChatRow/receipts/vote-pill components need `pokable: bool` +
`poke(string)` (or a parameterless `poke()` where the row knows its member)
threaded from the app root — same as MemberPill.

### Self-edit enforcement precedent

The wire drops what claims another member: `envelope.by != from` drops at
`cmd_net_delivered` (net.rs:1103-1106); `Declined`/`Withdrawn` bodies
claiming another member drop (net.rs:1486-1489, 1516-1520 — "the link
identity is the only proof of authorship"). The reserved-op guard keeps user
proposals out of membership ops (proposals.rs:443-448). Applied chain blocks
carry no `by` — the threshold carries the rule: honest voters only ever see
profile proposals that passed the ingest gate, so nothing else can collect m
signatures. Same trust model as set_image (agents-are-seats: security comes
from the threshold alone).

## Design

### 1. New ops (Organization surface, no new Command)

| op | payload | notes |
|----|---------|-------|
| `set_member_image` | `{op, member, value: <file name>, bytes_b64}` | bytes mandatory, decodable, square (h), within `payload_fits` |
| `remove_member_image` | `{op, member}` | offered only while an image is applied (alt-label gate like app.slint:7984) |
| `set_member_desc` | `{op, member, value}` | trimmed; ≤ `DESC_MAX` chars (d); empty value = clear |

`member` is the roster name, exactly as anchored — the same key
`restore_member`/`add_member` payloads already use.

### 2. Engine validation (proposals.rs)

- `validate_org_payload` gains three arms:
  - all three ops: `member` present and in `self.roster()` — but roster
    access needs `&State`, so the roster check lives in `cmd_propose` next
    to the reserved-op guard (443-448); `validate_org_payload` keeps the
    stateless checks (bytes present + decodable + square; desc length).
  - `set_member_image`: reuse `image_bytes` + `image_decodable`, plus the
    square check (new `fn image_dimensions(bytes) -> Result<(u32,u32)>`
    factored out of `image_decodable` so the dims are read once).
- **Self-edit, local**: in `cmd_propose`, a member-profile op whose
  `payload.member != self.member()` refuses:
  `BadPayload("only {member} can edit this profile")` — compact, one line.
- **Self-edit, wire**: drop (4) in the net.rs:1385 Proposed arm — a
  member-profile op with `payload.member != from` is dropped with a
  structured warn (`from=… claimed=…`), node-independent like drops 1-3.
  Wire twin of the square/decodable/fits checks: extend drop (2)'s condition
  to `set_member_image` (decodable + square).
- `validate_payload_fits` (198): extend the image-wording condition (209-210)
  to `set_member_image` so an oversized avatar refusal names the image, not
  the payload.
- The image-op helpers keyed on op strings must learn the new op where they
  key on `"set_image"`: `logo_ext` is generic (fine), `image_bytes` is
  payload-shape generic (fine).

### 3. LWW slots + checkpoint ripple (molt-core)

- `applied_lww_slot` return type changes `Option<&'static str>` →
  `Option<String>` (per-member slots are dynamic). Callers: engine
  chain.rs:680-681 (compare `Option<String>` — `.as_deref()` compare), core
  tests chain.rs:887-912. New arms:
  - `set_member_image` | `remove_member_image` → `Some(format!("member.image:{member}"))`
  - `set_member_desc` → `Some(format!("member.desc:{member}"))`
  - a profile op WITHOUT a member field → `None` (accumulate — conservative,
    can't mis-collapse two members into one slot).
  The `:` separator keeps slot strings injective for arbitrary member names
  (names can contain dots).
- **No canonical byte layout changes anywhere**: roster tags, republic-id,
  `molt-chain-checkpoint-v7`, `approval_bytes`/`block_link_bytes` all stay
  byte-identical. The only consensus-adjacent effect is the summary-rule
  divergence on mixed builds — open question (g).

### 4. Effective fold, avatar files, MemberView (engine)

- New fold `member_profiles(&self) -> BTreeMap<MemberId, MemberProfile
  { image: String, desc: String }>` over `applied_org_entries()`
  (proposals.rs:1336 — BORROWED values, never clone the bytes; same
  discipline as `org_effective`). Image reference mirrors the org rule
  (1309-1316): materialized file path with a storage dir, display value on
  a session-only open, cleared by `remove_member_image`.
- **File materialization**: `sync_avatar_files` beside `sync_logo_file`
  (session.rs:1439), called from `after_org_applied` and at open. Storage
  gains `set_avatar(stem: String, avatar: Option<(String /*ext*/, Vec<u8>)>)`
  + a `WriterMsg::Avatar` variant mirroring `WriterMsg::Logo`
  (molt-storage lib.rs:1386/2463): per-stem stale-file cleanup
  (`avatar-<stem>.*`), idempotent byte compare. File name:
  `avatar-{slugify(member)}-{fnv1a64(member):016x}.{ext}` — `slugify`
  (molt-core:663) keeps it readable and path-safe, the fnv suffix
  (molt-core:685; a file name, never key material) disambiguates slug
  collisions. Removal ops and members without an applied image get
  `set_avatar(stem, None)`.
- `MemberView` (molt-core:5131) gains `#[serde(default)] pub image: String`
  (file path or "") and `#[serde(default)] pub description: String`;
  `members_view` (proposals.rs:1675) fills them from `member_profiles()`.
  MCP `read_members` description (molt-mcp:850) gains one clause: "…, its
  vote-gated profile (image = local file path of the applied picture,
  description)".
- `Status` reply gains `#[serde(default)] image_budget: u64` = current
  `image_headroom(Organization, empty set_member_image payload, roster)` —
  the GUI's downscale target (b). Reused by the org-logo modal copy later if
  wanted.
- `change_summary` (304): the member ops' `current` comes from
  `member_profiles()` (image ref / desc of `payload.member`) — needs the
  fold's result at the call site, same as `OrgEffective` is passed today.
- `decision_summary` (966): labels `"set_member_image" => "Member picture"`,
  `"remove_member_image" => "Member picture removed"`,
  `"set_member_desc" => "Member description"`; content = the member name
  (image ops) / `"{member}: {value ≤160 chars}"` (desc). Bytes never in a
  chat line (987 rule).

### 5. GUI (molt-ui + .slint)

- **MemberRow** (theme.slint:157) gains `avatar: image`, `avatar-set: bool`,
  `desc: string`. Row height 40px → **80px** (decision f); 56px avatar box
  left of the name: placeholder `users.svg` colorized accent in an
  accent-soft box (mirror app.slint:5207-5214); pencil edit button ONLY on
  the own row (`m.name == root.node-member`), corner-overlaid like
  app.slint:5224-5236.
  **Vertical alignment is the load-bearing detail of the 80px row**: the
  presence dot, the avatar and the name sit on the FIRST line, so their
  containers are top-packed (`alignment: start` on the row's
  VerticalLayouts) and the dot centers against the name line's height, NOT
  against the row (`y: (parent.height - self.height) / 2` is exactly what
  must NOT be copied from the 40px row - it would float the dot beside the
  second description line). The description column is a fixed 2-line-height
  box (`2 * Theme.fs-body * 1.35`) with `wrap: word-wrap` and
  `overflow: elide`, full text on hover via HintTip.
  Watch the Slint collapse trap: the avatar box gets explicit width/height
  inside the HorizontalLayout, never x/y-anchored children in an
  `alignment: start` parent.
- **Avatar loading** (molt-ui, in the 5367-5380 mapping): a
  `HashMap<String, slint::Image>` cache keyed by file path in `ChatUiState`;
  decode by content (`image_from_bytes`), re-decode only when the path
  changes (mirror of the org rule at 5437-5445). `sync_rows` keeps patching
  rows in place.
- **Modals** (clone `ol-dlg`, app.slint:7978-8070):
  - `profile-img-modal-open` + `profile-img-draft` + callback
    `member-img-pick()` (same rfd body as `on_org_logo_pick`, molt-ui:1705)
    and `member-propose(op, member, value)`;
  - `profile-desc-modal-open` + a TextEdit + char counter (mirror the
    charter modal at app.slint:8160-8188).
- **`on_member_propose`** (molt-ui, mirror `on_org_propose` 1588-1691):
  `set_member_image` reads the file off the UI thread, decodes, center-crops
  square, downscales/re-encodes until the encoded size ≤ the engine-served
  `image_budget` (floor 128²; toast an honest error below that), embeds
  `bytes_b64` + `member`, proposes; toast rides the outcome.
- **Proposal cards**: extend molt-ui:5895 to
  `matches!(op, "set_image" | "remove_image" | "set_member_image" |
  "remove_member_image")` — the inline preview/save path (1766/1792) then
  works unchanged. Desc proposals render via the generic Ist/Soll pair.
- **Titles**: `display_title` (molt-ui:6720) gains a member-op arm like
  restore_member: EN "Picture: {member}" / "Description: {member}", DE
  "Bild: {member}" / "Beschreibung: {member}"; `remove_member_image` EN
  "Remove picture: {member}" / DE "Bild entfernen: {member}".
- **i18n**: every new user-visible string gets a kebab-case
  `in property <string> …;` in theme.slint `Strings` (pattern: `mem-poke`,
  theme.slint:985) + an EN/DE pair in `lexicon!` (molt-ui:8150; e.g.
  `mem_poke: "Poke member", "Mitglied anstupsen";` at 8537). The existing
  completeness test enforces the pairing. New keys (compact, no em dash,
  plain `-` only): `mp-img-title`, `mp-desc-title`, `mp-col-desc`,
  `mp-desc-count`, plus reuse of `ol-current`/`ol-none`/`ol-pick`/
  `ol-remove`/`oc-propose` where the meaning is identical.
- **Poke rollout**: work the site table above; Pattern A/B per site;
  `pokable` + poke callback threading for ChatRow, receipts row, vote pills,
  decided-table dots.

## TDD keystones (write red first, in this order)

1. **molt-core** `chain.rs` tests (extend 887-912):
   `member_profile_ops_occupy_per_member_slots` — set_member_image(walter)
   and set_member_image(petra) get DIFFERENT slots; set+remove for one
   member share; `set_member_desc` separate; a member-less profile payload
   accumulates. Red: today all return `None`.
2. **engine** proposals.rs size-gate twins (clone 1944-1993 with a
   `set_member_image` payload): headroom-is-accepted, one-quantum-over
   refused, refusal names the KiB. Plus
   `a_member_image_must_be_square` (non-square refused, square accepted) and
   `a_long_description_is_refused` (DESC_MAX edge).
3. **engine** self-edit: proposals.rs test
   `a_profile_op_for_another_member_is_refused` (cmd_propose); net-ingest
   twin `a_profile_proposal_claiming_another_member_is_dropped` beside the
   existing wire-drop pins (two-node loopback, e.g. in
   `crates/molt-engine/tests/two_instances.rs` style).
4. **engine e2e keystone** (loopback, chain-governed):
   member proposes own image+desc → threshold approves → applied → both
   nodes' `members_view` carry the desc and an existing avatar FILE with
   identical bytes; `remove_member_image` deletes the file. Also: reopen the
   workspace → `sync_avatar_files` rebuilds deterministically.
5. **engine checkpoint**: after a cut above several superseded avatars, the
   summarized `applied` keeps only the latest entry per member and the
   post-cut fold equals the live fold (beside the existing WP4b summary
   tests in engine chain.rs).
6. **molt-ui stub tests** (`CARGO_TARGET_DIR=target/dev-ui
   SLINT_LIVE_PREVIEW=1 cargo test -p molt-ui --lib --features
   molt-ui/live-preview`): display_title member arms EN+DE; `image_op` true
   for the new ops in `to_proposal_row`; lexicon completeness stays green.

## Execution order

1. molt-core: slot arms + return-type change (+ keystone 1) — ripple into
   engine chain.rs:680-681.
2. Engine ops: validation, self-edit local+wire, square, DESC_MAX,
   fits-wording, decision_summary/change_summary (+ keystones 2, 3).
3. Engine state: `member_profiles` fold, `sync_avatar_files` +
   storage `set_avatar`, MemberView/Status additions, read_members
   description (+ keystones 4, 5).
4. molt-ui logic: member_propose, avatar cache, row mapping, display_title,
   lexicon (+ keystone 6) — iterate on the stub suite.
5. .slint: members table rework, the two modals, card extension, poke
   rollout per the site table.
6. Review the diff, then land green on master.

## Verification

- Engine/core: `cargo test -p molt-core -p molt-engine` (clippy rule:
  `.expect("…")` in tests, never `.unwrap()`).
- GUI iteration: `scripts/dev-ui.sh build` + the stub test suite (command
  above, ~11 s) — NOT the window build.
- Once per change-set: `cargo build -p molt-ui-window -p molt-ui`
  (~4 min/~6 GiB; `-j 1` when RAM is tight, never two window builds
  concurrently) + `cargo clippy --all-targets` clean.
- `python3 scripts/check-doc-refs.py` (this doc cites paths).
- Optional hand test: the MCP headless recipe (two nodes over
  `dev_relay`) — propose `set_member_image` via the generic `propose` tool
  and read it back with `read_members`.
