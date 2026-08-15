# Buzz (`block/buzz`) — UI feature review

Date: 2026-08-01. Subject: <https://github.com/block/buzz> at commit
`3d7712cc36e8da563cb1c121fc58bfc505d38496` (v0.5.3), Apache-2.0, Block Inc.

Companion to `docs_archive/reviews/buzz_comparison.md`, which compared the
*protocol and architecture*. This document compares the **product surface** —
what a person can actually do in their app that they cannot do in ours.

## 1. Method, and how this differs from the first review

The first review was written from their published documents; their source was
not read. **This one is read from the source.** The repository was cloned and
their `desktop/` and `web/` frontends were read directly: feature layout,
component implementations, the shared primitive library, their keyboard-shortcut
table, their notification-decision logic, their search-operator parser, their
audio pipeline. Where a claim below is an inference rather than something read,
it says so.

Scope, per instruction: **their desktop app and their web app**. `admin-web/`
and `mobile/` are out of scope — we ship neither. Mobile is mentioned exactly
once, where it carries something a desktop-only product would still want.

Verdicts are one of three:

- **STEAL** — the idea transfers as-is. Nothing in it depends on their trust
  model. The only question is our effort.
- **ADAPT** — worth having, but their implementation leans on a plaintext relay
  or a hosted operator. We would rebuild the mechanism locally.
- **SKIP** — it exists only because a server reads content, or because they
  have a product we deliberately do not have.

A verdict is a recommendation to a planning session, not a decision.

## 2. Scale and shape

|                     | Buzz desktop                                     | MoltRepublic GUI                      |
| ------------------- | ------------------------------------------------ | ------------------------------------- |
| Stack               | Tauri 2 + React 19 + TypeScript + Tailwind       | Slint + Rust                          |
| UI source           | ~1 190 files, ~245 000 lines (excl. tests)       | 5 `.slint` files (13 300 lines) + `molt-ui` (7 400 lines) |
| Feature modules     | 28                                                | 6 surfaces × views                    |
| Second frontend     | `web/` — repo browser + invite landing            | none                                  |
| Component library   | Radix primitives + ~70 shared components          | ~30 hand-built Slint components       |

The ratio is roughly **12:1** in UI code. That is the honest frame for
everything below: they are not smarter, they are a much larger team shipping a
much larger product. Most of the gap is volume, not insight. The list below
tries to separate the two — a handful of these are genuinely good ideas, and
most of the rest are simply work.

Their `web/` app is deliberately thin: a repository browser (`repos`,
`repos/$repoId/blob/*`) and an invite-redemption landing page (`invite/$code`).
It is the public face of their "your domain is your workspace" story
(`VISION_SOVEREIGN.md`), not a second client. **Nothing in `web/` is worth
taking** — the whole premise is a relay that serves rendered HTML of plaintext
content to anonymous browsers, which is the exact inverse of our product.

## 3. What we already have

The gap list is only useful if it is honest about the baseline. Verified in our
tree, not assumed:

- Six surfaces (Organization / Chat / Memory / Quests / Vault / Wallet) with
  per-surface views, and a nav that carries unread and pending counts.
- Chat: channels as views, reactions with an emoji grid, quoting, delete,
  per-message read receipts (they have no equivalent), file share/download with
  progress, an uploads table, retention windows, paging.
- Drafts (persisted per compose box), presence, status pills, member pills.
- Proposal cards with approve/decline, pending/accepted/declined views, the
  chain surfaces.
- Full ritual UX: founding, join with charter ratification, recovery from both
  sides, seed/backup panels, restore ways.
- Settings that are real and persisted: language (i18n via `Strings`), theme
  (light/dark, WCAG-checked palette), two alert-sound slots, the relay pool
  editor with the clearnet exposure acknowledgement, workspace encryption,
  export/import, auto-backup, Tor modes.
- A splitter-resizable layout, hover tips, toasts, confirm modals, a spinner.

That is a serious application. The gaps below are real, but they are gaps in a
working product, not holes in a mock.

## 4. The gap

### A. Findability — the largest single gap

| # | Feature | Where in their tree | Verdict |
|---|---------|---------------------|---------|
| A1 | **Global search** with Slack-style operators — `from:`, `in:`, `after:YYYY-MM-DD`, `before:` — parsed out of free text, remainder used as a full-text query | `features/search/lib/parseSearchOperators.ts` | ADAPT |
| A2 | **⌘K quick-search dialog** over channels, people and messages, with skeletons while loading | `features/search/ui/TopbarSearch.tsx` | STEAL |
| A3 | **Find-in-channel bar** (⌘F), scoped to the open conversation | `features/search/ui/ChannelFindBar.tsx` | STEAL |
| A4 | **Channel browser** — a directory of channels you are not in, with join | `features/channels/ui/ChannelBrowserDialog.tsx` | STEAL |
| A5 | **Back / forward navigation** (⌘\[ / ⌘\]) with history state | `shared/hooks/useHistorySearchState.ts` | STEAL |
| A6 | **Jump-to-unread** — an unread pill and a "more unread" button in the sidebar | `shared/ui/UnreadPill.tsx`, `features/sidebar/ui/MoreUnreadButton.tsx` | STEAL |

**We have no search at all.** Zero occurrences of the concept in the GUI layer.
For a product whose entire pitch is a durable shared brain — Memory notes,
Quests, an archive of chat within the retention window — this is the most
expensive missing feature on the list. A republic that has been running for six
months has knowledge in it that is currently unreachable except by scrolling.

The ADAPT on A1 is the important nuance: **their search runs on the relay.**
The parser produces an FTS query and NIP-01 `since`/`until` filters that go to
the server, because the server holds their plaintext. We cannot do that and must
never try. Ours has to be a **local index over the local event log**, built as
events are applied. That is not a downside — a local index can search Memory,
Quests, Vault metadata, the chain and chat in one pass, which their split model
cannot. Library note per CLAUDE.md: do not hand-roll this. `tantivy` is pure
Rust and fits our posture; SQLite FTS5 would drag `libsqlite3-sys` (C) into the
default build and is therefore disqualified by the standing rule. For a first
cut, a simple in-memory inverted index over the retained window may beat both on
effort — but that comparison belongs in the plan, argued, not assumed.

### B. Chat depth

Our chat is a flat, single-level, plain-text timeline. Theirs is a full modern
messenger. Item by item:

| # | Feature | Where | Verdict |
|---|---------|-------|---------|
| B1 | **Threads** — a real side panel, thread summary rows in the timeline, follow/unfollow a thread, load-missing-ancestors, an independent detachable thread panel | `features/messages/lib/threading.ts`, `ui/MessageThreadPanel.tsx`, `useThreadFollows.ts` | STEAL |
| B2 | **Mentions** — `@user` autocomplete with ranking, `#channel` autocomplete, `:emoji:` autocomplete, mention highlighting, a warning dialog when you mention a non-member | `lib/mentionRanking.ts`, `ui/MentionAutocomplete.tsx`, `ui/NonMemberMentionDialog.tsx` | STEAL |
| B3 | **Rich-text composer** — TipTap/ProseMirror: bold, italic, code, code blocks, lists, links with an inline link editor, and a **spoiler mark** | `lib/useRichTextEditor.ts`, `ui/FormattingToolbar.tsx`, `lib/spoilerMark.ts` | ADAPT |
| B4 | **Selection formatting tray** — a floating toolbar on text selection | `ui/SelectionFormattingTray.tsx` | STEAL |
| B5 | **Markdown rendering** in messages, with `shiki` syntax highlighting for code blocks and GFM tables | `shared/ui/markdown/`, dep `shiki`, `remark-gfm` | ADAPT |
| B6 | **Message action bar on hover** — react, reply, thread, copy link, edit, delete, remind me, mute thread, mark unread, report | `ui/MessageActionBar.tsx` | STEAL |
| B7 | **Message permalinks** — every message has a copyable link that resolves in-app | `lib/messageLink.ts`, route `channels.$channelId.posts.$postId` | STEAL |
| B8 | **Editing a sent message** | `ui/ComposerReplyEditBanner.tsx` | ADAPT |
| B9 | **Typing indicators** — throttled broadcast (kind 20002, one publish per 3 s), rendered as a timeline row | `useTypingBroadcast.ts`, `ui/TypingIndicatorRow.tsx` | STEAL |
| B10 | **Day dividers and an unread divider** in the timeline | `ui/DayDivider.tsx`, `ui/UnreadDivider.tsx` | STEAL |
| B11 | **Custom emoji** — NIP-30, per-member sets, a read-only community palette that is the union of all members' sets with a deterministic winner on shortcode collision | `features/custom-emoji/` | STEAL |
| B12 | **Link previews / unfurls** with per-service cards | `shared/ui/link-preview-attachment.tsx` | SKIP |
| B13 | **Image lightbox, carousel, video player** with aspect handling, download and a context menu | `shared/ui/SimpleImageLightbox.tsx`, `VideoPlayer.tsx`, `carousel.tsx` | STEAL |
| B14 | **Composer image editor** — crop/annotate before sending | `ui/ComposerImageEditor.tsx` | ADAPT |
| B15 | **Diff messages** — a posted patch renders as a real side-by-side diff, expandable | `ui/DiffMessage.tsx`, `ui/DiffViewer.tsx`, dep `react-diff-view` | ADAPT |
| B16 | **Cross-channel drafts panel** — every unsent draft in one list with a detail pane | `ui/DraftsPanel.tsx`, `ui/DraftDetailPane.tsx` | STEAL |
| B17 | **Virtualized timeline** with anchored scroll, upward pagination on wheel, buffered prepends, skeletons | `ui/useAnchoredScroll.ts`, `useVirtualizedBottomSettle.ts`, `TimelineSkeleton.tsx` | ADAPT |
| B18 | **Wave** — a one-click "X waved at you" nudge message | `lib/waveMessage.ts` | STEAL |

Notes on the non-obvious verdicts.

**B3/B5 (rich text, markdown).** There is no TipTap for Slint. This is the one
place where their stack does real work for them that ours will not. Our Memory
surface already has a pseudo-markdown renderer (`surfaces.slint`, block kinds
0–4) written by hand — extending *that* is the wrong instinct. The rule applies:
`pulldown-cmark` is the obvious Rust parser, and highlighting has real
candidates (`syntect`, or `inkjet`/tree-sitter). The plan must argue which, and
must be explicit that a *rich-text editor* (WYSIWYG) and a *markdown renderer*
are two different projects. The renderer is worth doing; the WYSIWYG editor
probably is not — markdown-in, rendered-out with a formatting toolbar that
inserts syntax is 80 % of the value for 20 % of the work.

**B12 (unfurls) is a SKIP on privacy grounds, not effort grounds.** Fetching a
preview for a pasted link means every member's client makes an HTTP request to
an attacker-chosen URL the moment a message arrives. That is a deanonymisation
primitive aimed straight at the thing we protect. If it is ever wanted, it must
be sender-side only (the sender fetches, the *card* travels inside the encrypted
message) and off by default. Write that down before anyone builds it.

**B15 (diff messages)** is worth more to us than it looks. We are not a code
forge, but every gated proposal is a *change*, and rendering a Memory-note
proposal as a real before/after diff instead of a wall of new text is directly
on our path. That reframing is the steal here, not the code-review use case.

**B17 (virtualization).** We page instead (`PagerRow`). Slint's `ListView` is
virtualized, so this is reachable; the hard part they solved is scroll anchoring
across prepends, which is a genuine source of bugs and worth reading their
implementation for.

### C. Notifications

| # | Feature | Where | Verdict |
|---|---------|-------|---------|
| C1 | **A notification decision function** — notify on mentions, on replies in threads you participated in / follow / authored, suppressed by muted thread roots and muted channels | `features/notifications/lib/shouldNotify.ts` | STEAL |
| C2 | **OS desktop notifications** | `lib/desktop.ts`, `@tauri-apps/plugin-notification` | STEAL |
| C3 | **Per-category sounds** — 12 named sounds, assignable per slot (`dm`, `mention`, `thread_reply`, `needs_action`, …) | `lib/sound.ts` | STEAL |
| C4 | **Mute a channel / mute a thread** | `ui/MessageActionBar.tsx` | STEAL |
| C5 | **App badge / home badge count** | `lib/homeBadge.ts` | STEAL |

We have two sound slots (message, vote) and three sounds, and **no OS
notifications at all** — the app is only useful while it is the focused window.
For a governance product where the thing being waited on is *other people's
approvals*, that is a product-level gap, not a polish gap: a proposal that needs
a threshold can sit unseen for a day because nobody's OS told them.

C1 is the item to actually copy, because it is the hard part. Everyone builds
notifications; the reason theirs is not annoying is that a single pure function
decides, from a small explicit set of inputs, whether an event deserves an
interrupt. Ours would take the same shape with different inputs: **a proposal
that needs my signature outranks everything else**, and the muted-set concept
maps to channels and quest threads.

### D. The Home inbox

Their `Home` is not a landing page, it is a **triage queue**: one list of
everything wanting attention, with a detail pane, filtered by

`All · Projects · Mentions · Threads · Needs action · Agents · Reminders · Drafts`

(`features/home/ui/InboxFilterMenu.tsx`), with read state, auto-selection of the
next item, and a persistent selection anchor.

**Verdict: STEAL, and the highest-value single idea in this review for us.**

Our nav is organised by *where things live* (six surfaces). Theirs adds a view
organised by *what wants me* — and for a republic, the canonical answer to
"what wants me" is exact and short: **proposals awaiting my signature**,
mentions, replies to what I proposed, files offered to me, a join or recovery
request. We already compute nearly all of that (the nav badges carry the
counts). What is missing is the single screen that puts it in one list and lets
a member clear it. A "Needs action" filter over a threshold-governed workspace
is a stronger feature for us than it is for them.

### E. Reminders

Remind-me-later on any message, with shared time presets ("In 30 minutes",
"In 1 hour", "In 3 hours", "Tomorrow at 9am", "Next Monday at 9am"), a snooze
menu, due-reminder notifications, a reminders panel, and an inbox filter. The
presets are one module used by both the create dialog and the snooze dropdown —
`features/reminders/lib/timePresets.ts` — and each preset is guaranteed to
return a future timestamp (the 9am ones roll to the next day if 9am has passed).

Stored as an encrypted per-user event (their NIP-ER). **Verdict: STEAL** — and
for us it is *easier*, because a reminder is private to one member and therefore
never touches the chain, never needs a threshold, and never leaves the device.
It is the cheapest genuinely-modern feature on this list.

### F. Presence and identity

| # | Feature | Where | Verdict |
|---|---------|-------|---------|
| F1 | **Avatars, everywhere** — in the timeline, member lists, popovers, DM intro stacks | `shared/ui/UserAvatar.tsx` | STEAL |
| F2 | **Identicons for agents** — deterministic generated avatars (`jdenticon`) | `ui/BotIdenticon.tsx` | STEAL |
| F3 | **User status** — emoji + text with presets ("In a meeting", "Commuting", "Out sick", "Vacationing", "Working remotely"), clearable | `features/user-status/ui/SetStatusDialog.tsx` | STEAL |
| F4 | **Profile popover on any name/avatar**, and a full profile panel with tabs | `ui/UserProfilePopover.tsx`, `ui/UserProfilePanel.tsx` | STEAL |
| F5 | **Pubkey component** — consistent truncation and copy everywhere a key is shown | `shared/ui/PubKey.tsx` | STEAL |
| F6 | **Animated avatars** — record from the webcam, segment the person from the background with MediaPipe, compose over a backdrop, encode a ping-pong APNG | `features/profile/ui/AnimatedAvatarCapture.tsx`, deps `@mediapipe/tasks-vision`, `upng-js` | SKIP |

**We have no avatars at all** — one occurrence of the word in the whole GUI
layer. Members are text pills. This is the cheapest large perceived-quality win
available to us, and F2 shows the trick that avoids the whole upload/storage
problem: a deterministic identicon derived from the member's public key needs no
image pipeline, no relay storage, no moderation, and is *cryptographically
meaningful* — two members with visually different identicons provably have
different keys. For a product where identity is a keypair, an identicon is not
decoration, it is a **key fingerprint a human can compare at a glance**. That is
a better fit for us than a photo, and it should probably ship *before* any
upload path exists.

F6 is a SKIP: enormous effort, needs a C-heavy vision dependency, and the
webcam is not a peripheral we want this product asking for.

### G. Approval and automation UI — the most transferable design

Buzz's `workflows` feature (a preview flag) is YAML-defined automation with
**approval gates**. Three pieces of its UI are directly applicable to us:

1. **`WorkflowApprovalCard`** — when a run reaches a gate, an approval card
   appears *inline in the channel*, amber-bordered, with an optional note field
   and approve/reject. It renders nothing once the approval is resolved or
   expired. Approval happens where the conversation is, not in a separate
   console.
2. **`WorkflowRunTrace`** — a run renders as an ordered list of steps, each with
   a status badge from a fixed vocabulary: `completed` · `failed` · `running` ·
   `pending` · `cancelled` · `skipped` · `waiting_approval`. You read the state
   of a multi-step process in one glance.
3. **`WorkflowFormBuilder`** — a form that round-trips to YAML
   (`formStateToYaml` / `yamlToFormState`), so the same automation can be edited
   as a form or as text.

**Verdict: ADAPT, high value.** Their engine is not ours and their trust model
is not ours — but *we are a governance product and they have better approval
UI than we do*. Two concrete transfers:

- **Approvals belong in the conversation.** Our proposal cards live on the
  gated surfaces; the discussion lives in chat. Their pattern says put the
  actionable card inline where the deliberation is happening, with the vote
  buttons on it. We already thread a 💬 back-link from an applied change to its
  discussion — this is the same link walked the other way, and it is the more
  useful direction.
- **A proposal is a run with steps.** Proposed → collecting signatures (m of n,
  with the signers named) → sealed at threshold → committed to the chain →
  applied. Today that is spread across pending/accepted views and a signature
  count. A single trace with the same fixed status vocabulary would make the
  threshold legible, which is the one thing a new member most needs to
  understand about this product.

Their expiry handling is also worth noting: the card disappears when the
approval expires. Our proposals have no expiry concept in the UI at all.

### H. Long-form and knowledge surfaces

| # | Feature | Where | Verdict |
|---|---------|-------|---------|
| H1 | **Channel canvas** — every channel has one editable markdown document attached to it, view/edit toggle, with channel-name autocomplete | `features/channels/ui/ChannelCanvas.tsx` | STEAL |
| H2 | **Forum channels** — long-form threaded posts with a composer, post cards, a thread panel; a channel *type*, not a separate app | `features/forum/` | ADAPT |
| H3 | **Pulse** — a notes timeline with tabs (`search / everyone / people / liked / agents`), likes, replies, and agent-activity cards | `features/pulse/` | SKIP |
| H4 | **Ephemeral / temporary channels** — a first-class channel type chosen at creation | `ui/ChannelTypePicker.tsx` | STEAL |
| H5 | **Channel templates** | `features/channel-templates/` | ADAPT |

H1 is the sleeper. A canvas is *one durable document per channel* — the
charter-adjacent, always-current summary that chat scrolls away from. We have
exactly the right machinery for this already: Memory is versioned cross-linked
notes, and gating is per-surface. A canvas is a Memory note pinned to a channel,
and it fills a real hole (what is this channel *for*, decided and current,
without reading 400 messages).

H4 costs almost nothing and matters for a privacy product: a channel that is
explicitly ephemeral sets the right expectation about what is retained, and we
already have retention windows to implement it against.

H3 is a SKIP — a social timeline with likes is scope creep for a republic, and
"everyone" as a tab implies a public surface we do not have.

### I. Onboarding, backup and recovery UX

This is the area where their UI craft most exceeds ours on ground we already
occupy — we have all of these flows, theirs are simply better presented.

- **A staged onboarding flow** with slide transitions and consistent chrome
  (`OnboardingFlow.tsx`, `OnboardingSlideTransition.tsx`, `OnboardingChrome.tsx`).
- **`BackupPasswordTimeline`** — an animated explanation of what the backup
  password protects and when it is needed, honouring `prefers-reduced-motion`.
- **`BackupTestFlow` — the standout.** After creating a backup, the app makes
  you *prove it works*: drop the backup file back in, type the password, and the
  app verifies it decrypts. Progress through the test survives navigating away;
  the password attempt deliberately does not persist and is cleared on submit or
  unmount.
- Dedicated screens for every degraded state: `KeyringLockedScreen`,
  `RecoveryScreen`, `RelaunchRequiredScreen`, `ResetFailedScreen`,
  `MembershipDenied`, `PendingInviteGate`, `JoinPolicyNotice`.
- **`NsecMaskedDisplay`** — the secret key is masked by default with an explicit
  reveal.

**Verdict: STEAL the backup test flow specifically.** We ship encrypted
workspaces, auto-backup and seed panels, and — like everyone — we currently
*tell* the user their backup is fine. An untested backup is not a backup, and we
are a product where a lost key means a lost republic. Making the user
successfully restore once, at the moment they create it, converts a promise into
a verified fact. Their detail about not persisting the password attempt is
correct and should be copied exactly.

The masked-secret display is also directly relevant to our standing rule about
never pointing at secrets.

### J. Application hygiene

| # | Feature | Where | Verdict |
|---|---------|-------|---------|
| J1 | **Feature-flag manifest** — `preview-features.json` validated with a Zod schema at startup; on parse failure it falls back to an empty manifest so gated UI stays hidden and nothing leaks. `FeatureGate` component + `useFeatureEnabled` hook; users toggle preview features in settings | `shared/features/`, `preview-features.json` | STEAL |
| J2 | **In-app updater** — check, download, install and relaunch, with a sidebar indicator | `settings/UpdateChecker.tsx`, `UpdaterProvider.tsx` | ADAPT |
| J3 | **Keyboard-shortcut system** — a declarative table by category (Navigation / Zoom / Messages), platform-aware key rendering, surfaced read-only in settings | `shared/lib/keyboard-shortcuts.ts`, `settings/ui/KeyboardShortcutsCard.tsx` | STEAL |
| J4 | **Zoom** (⌘+ / ⌘− / ⌘0) | same | STEAL |
| J5 | **Send feedback** dialog | `settings/ui/SendFeedbackDialog.tsx` | SKIP |
| J6 | **Prevent sleep** while agents are running | `settings/ui/PreventSleepSettingsCard.tsx` | STEAL |
| J7 | **Adaptive theme engine** — derives the whole palette from four key colours of a syntax theme, detecting light/dark from background luminance; plus a live theme preview frame | `shared/theme/adaptive-theme.ts` | ADAPT |

J1 is the one to take seriously as *process*, not as a feature: five of their
biggest surfaces (workflows, projects, pulse, forum, agent-managed profiles)
ship disabled behind a manifest. That is how a small team lands large,
half-finished surfaces on the main branch without shipping them to users — which
is precisely the situation we are in with several of ours. The fail-closed
schema validation is the right default and costs nothing.

J3 is embarrassing to write down: **we have essentially no keyboard support** —
twelve focus-related occurrences in 13 300 lines of Slint, no shortcut table, no
command palette. A desktop application that cannot be driven from the keyboard
reads as a prototype to exactly the kind of user we are building for.

J5 is a SKIP: feedback dialogs phone home.

### K. Structure and organisation

- **Sidebar sections with drag-and-drop reordering** (`@dnd-kit`), custom
  sections, a pinned header, per-channel context menus
  (`features/sidebar/ui/SidebarDnd.tsx`). Our nav order is fixed.
- **Community switcher rail** for multiple communities
  (`features/communities/ui/CommunitySwitcher.tsx`). We have multiple workspaces
  but no persistent rail to switch between them — worth comparing.
- **Relay connection card in the sidebar** — connection state is always visible,
  not buried in settings (`ui/SidebarRelayConnectionCard.tsx`). We have a header
  transport pill; theirs is more prominent and more informative. Given our relay
  pool with per-relay confirmation state, a compact always-visible summary is a
  good fit. **STEAL.**

### L. Polish, motion and accessibility

Their shared library carries the things that make an app feel finished:
skeletons and shimmer for every loading state, virtualized lists, `sonner`
toasts, an escape-key surface stack so nested overlays close in the right order
(`shared/hooks/escapeSurfaces.ts`), scroll-boundary locking, deferred modal
open, smooth corners, and deliberate micro-delight — an emoji burst on
reaction, a "poof" burst on delete, spoiler particles.

Two of these are not cosmetic:

- **Accessibility.** Their components are built on Radix and carry `aria-*`
  throughout. Our Slint tree has **zero `accessible-role` / `accessible-label`
  annotations** — Slint supports both, and without them the app is opaque to a
  screen reader. This is cheap to fix and currently a hard blocker for any user
  who needs it.
- **Reduced motion.** They honour `prefers-reduced-motion` in their animated
  components. We have almost no animation, so this is not yet a debt — but it
  becomes one the moment we add any.

## 5. What we must not copy

Recorded so it is not re-litigated per work package. Each of these is a good
feature *for their product* and unusable in ours:

- **Relay-side search.** Their FTS runs where the plaintext is. Ours must be
  local, always.
- **Moderation as an operator function** — NIP-56 reports, a moderation queue, a
  timeout with a duration submenu, a composer timeout banner. This presumes an
  operator with authority over members. We have no operator; our only legitimate
  moderation is a threshold decision by the members. Copying the *UI* of a
  report queue would import a governance model we have explicitly rejected.
- **Hosted communities and the community-application flow.** Same reason.
- **Mesh compute** (`VISION_MESH.md`) — pooling members' GPUs means members'
  prompts run on other members' hardware. Their own consent screen says so
  honestly. Not for us.
- **Remote agents** (`VISION_REMOTE_AGENTS.md`) — deploying an agent to a
  cluster with its key.
- **The projects forge** — issues, PRs, inline code review, contribution graphs,
  a git server on the relay. A large, well-built product we are not building.
  The *diff rendering* (B15) and the *approval trace* (§G) are the parts worth
  extracting.
- **Link unfurling** as they do it — receiver-side fetch, see B12.
- **Push gateway / mobile pairing.** No mobile client.

## 6. What we have that they do not

Not a scorecard — it matters because it says which gaps are worth closing and
which would dilute what makes this product different.

- **End-to-end encryption of everything.** Their relay reads content; that is
  the enabling assumption behind their search, their moderation and their web
  view.
- **Threshold governance as the state model.** Their approval gates are a
  workflow feature; our m-of-n signed chain is the product. They have no
  equivalent of a persistent-change chain, a sealed roster, or sign-what-you-see.
- **Per-message read receipts.** Their action bar has mark-read/unread; they have
  no per-message receipt from other members.
- **A founding ritual** with charter deliberation and ratification, and a
  recovery ritual with UI on both sides.
- **Tor-first networking** with a fail-closed dialer and an explicit clearnet
  exposure acknowledgement in the relay editor.
- **Encrypted-at-rest workspaces** with export/import and auto-backup.
- **A vault and a multisig wallet surface** — no analogue at all.
- **One command set driving every frontend**, with a test that enforces MCP/GUI
  co-equality.

## 7. Recommended shortlist

Ranked by value per unit of effort, for a planning session to cut down. Nothing
here is started, and none of it should be started before the current Nostr work
(N4b, N5) lands.

| Rank | Package | Why it is first | Rough size |
|------|---------|-----------------|------------|
| 1 | **Identicon avatars** (F2, F1) | Largest perceived-quality gain per line of code; deterministic from the member key, so no storage, no upload, no moderation — and it doubles as a human-comparable key fingerprint | S |
| 2 | **OS notifications + a `should_notify` decision function + per-slot sounds** (C1–C5) | A governance app whose whole point is waiting on other people's signatures currently cannot tell you when one arrives | S–M |
| 3 | **The "Needs action" inbox** (§D) | One screen answering "what wants me", where our answer is unusually crisp: proposals awaiting my signature | M |
| 4 | **Keyboard system + ⌘K quick switcher** (J3, J4, A2, A5) | Cheap, and its absence is what makes the app read as a prototype | M |
| 5 | **Local search** (A1, A3) | The biggest functional hole; must be local-index, library choice argued in the plan | L |
| 6 | **Backup test flow** (§I) | Converts our central safety promise into a verified fact; small and self-contained | S |
| 7 | **Reminders** (§E) | Fully local, no chain, no threshold — genuinely modern for very little | S |
| 8 | **Proposal-as-run-trace + inline approval cards** (§G) | Makes the threshold legible, which is the hardest thing about this product to explain | M |
| 9 | **Markdown rendering in chat + threads** (B5, B1) | The two biggest chat gaps; both are real projects, not afternoons | L each |
| 10 | **Accessibility pass** (`accessible-role` / `accessible-label`) (§L) | Cheap, and currently a hard blocker for anyone who needs it | S |
| 11 | **Feature-flag manifest** (J1) | Process, not product: lets large surfaces land on master unshipped | S |
| 12 | **Channel canvas** (H1) | Fills a real hole and reuses Memory almost as-is | M |

## 8. Open questions for the planning session

1. **Is search local-index-only, and which library?** The privacy answer is
   forced; the implementation is not. `tantivy` vs. a hand-rolled inverted index
   over the retained window — the plan must argue it, and must state that SQLite
   FTS5 is disqualified by the ring/C-free default-build rule.
2. **Do we want threads at all, or are channels our threading?** Channels are
   already views, not boundaries. Threads may be a second axis we do not need —
   this is a product decision, not a technical one, and it should be made before
   any of B1's machinery is built.
3. **Rich-text editor, or markdown-in / rendered-out?** The recommendation above
   is the latter. Confirm before anyone starts, because the two have an order of
   magnitude between them in effort.
4. **Does the inbox get its own surface, or is it a view on Organization?**
   Adding a seventh surface touches the `Surface` enum, the MCP co-equality
   test, and the nav — worth deciding deliberately.
5. **Should proposals have an expiry?** Their approval cards expire. Ours never
   do. That is a governance question, not a UI question, and it should be
   answered before the trace UI is designed around it.

---

*Provenance: read from `block/buzz` at `3d7712cc36e8`, 2026-08-01. Their source
was read for the claims above; where an implementation detail is quoted it was
read from the file named next to it. Nothing in this document proposes copying
Apache-2.0 source — see `docs_archive/reviews/buzz_comparison.md` §2 for the
licensing analysis that applies if that ever changes.*
