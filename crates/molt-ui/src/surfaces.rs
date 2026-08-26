// SPDX-License-Identifier: GPL-3.0-or-later
//! The surfaces projection: the plain, `Send` bundle every mirror pass
//! gathers off the UI thread (status, tables, proposal cards, chat rows),
//! the UI-local chat-bus state it is keyed on, and the row projections
//! (proposal cards, chain history, display titles) the window renders.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use molt_core::{ChannelRef, Command, ProposalView, Reply, Surface, SurfaceSnapshot};
use molt_engine::WalletHandle;
use slint::{ModelRc, VecModel};

use crate::channels::{
    channel_display_label, channel_key, derive_channels, known_titles, selected_channel_closed,
    selected_channel_org, update_known_proposals, KnownProposal,
};
use crate::chat_log::{
    annotate_chat_log, chat_line, chat_messages, merge_by_time, patch_system_lines, quote_sources,
    ChatViewCtx,
};
use crate::images::avatar_cache_key;
use crate::labels::{
    expires_label, file_date_label, file_size_label, never_seen_label, seen_label, strings_pick,
    surface_name, unix_now, when_label,
};
use crate::{ChainRow, MemberVoteMark, ProposalRow, RelayChange};

/// Rows per page of the proposal-outcome lists (Organization → Declined
/// and the gated surfaces' applied log). Below this the pager row hides.
pub(crate) const LIST_PAGE_SIZE: usize = 15;

/// The pure paging window: `(start, end, page, page_count)` over a list of
/// `len` rows, `size` per page. The requested 0-based `page` clamps into
/// range (a shrunk list re-bases onto its last page instead of showing an
/// empty one), and an empty list is one empty page — `page_count` is never
/// zero, so "page x of y" stays well-formed.
pub(crate) fn page_slice(len: usize, page: usize, size: usize) -> (usize, usize, usize, usize) {
    let page_count = len.div_ceil(size).max(1);
    let page = page.min(page_count - 1);
    let start = page * size;
    let end = (start + size).min(len);
    (start, end, page, page_count)
}

/// The bundle's effective page for one paged list (missing key = page 0).
pub(crate) fn page_of(pages: &HashMap<String, usize>, surface: &str, list: &str) -> usize {
    pages.get(&format!("{surface}:{list}")).copied().unwrap_or(0)
}

/// Toggle-or-switch a table's sort state: clicking the active column again
/// flips the direction, a new column starts ascending.
fn toggle_sort(active: &mut String, ascending: &mut bool, column: &str) {
    if active == column {
        *ascending = !*ascending;
    } else {
        *active = column.to_string();
        *ascending = true;
    }
}

/// Sort the Organization → Uploads rows by a header column key ("user" /
/// "date" / "file" / "type" / "size" / "checksum" / "download" /
/// "expires"); an empty or unknown key keeps the engine order. Text
/// columns compare case-insensitively; date/size/expiry sort by the
/// underlying numeric keys carried on the row — never the rendered label.
pub(crate) fn sort_uploads(rows: &mut [UploadRowData], column: &str, ascending: bool) {
    match column {
        "user" => rows.sort_by_key(|r| r.user.to_lowercase()),
        "date" => rows.sort_by_key(|r| r.ts),
        "file" => rows.sort_by_key(|r| r.name.to_lowercase()),
        "type" => rows.sort_by_key(|r| r.kind.to_lowercase()),
        "size" => rows.sort_by_key(|r| r.bytes),
        "checksum" => rows.sort_by_key(|r| r.checksum_full.to_lowercase()),
        "download" => rows.sort_by_key(|r| r.status.to_lowercase()),
        "expires" => rows.sort_by_key(|r| r.expires_ts),
        _ => return,
    }
    if !ascending {
        rows.reverse();
    }
}

/// Keep the uploads rows whose user, filename or checksum contains
/// `needle` case-insensitively; an empty needle keeps every row. The
/// checksum matches on the full sha256 hex, so a pasted full checksum
/// finds its row even though the cell shows a shortened prefix.
pub(crate) fn filter_uploads(rows: Vec<UploadRowData>, needle: &str) -> Vec<UploadRowData> {
    if needle.is_empty() {
        return rows;
    }
    let needle = needle.to_lowercase();
    rows.into_iter()
        .filter(|r| {
            r.user.to_lowercase().contains(&needle)
                || r.name.to_lowercase().contains(&needle)
                || r.checksum_full.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Sort the Organization → Members rows by a header column key ("name" /
/// "id" / "pk" / "last" / "uploads"); an empty or unknown key keeps the
/// roster order. Unanchored (empty) id/pk cells sort last ascending;
/// "last" orders by the REAL last-seen stamp, most recent first, with
/// never-seen members at the end.
pub(crate) fn sort_members(rows: &mut [MemberRowData], column: &str, ascending: bool) {
    match column {
        "name" => rows.sort_by_key(|r| r.name.to_lowercase()),
        "id" => rows.sort_by_key(|r| (r.id.is_empty(), r.id.to_lowercase())),
        "pk" => rows.sort_by_key(|r| (r.pk.is_empty(), r.pk.to_lowercase())),
        "last" => rows.sort_by_key(|r| (r.last_ts == 0, std::cmp::Reverse(r.last_ts))),
        "uploads" => rows.sort_by_key(|r| r.uploads),
        _ => return,
    }
    if !ascending {
        rows.reverse();
    }
}

/// Plain, `Send` snapshot of all surfaces, built off the UI thread.
pub(crate) struct SurfacesBundle {
    /// Language the labels were rendered for (0 = en, 1 = de) — the nav's
    /// sub-view names are localized when the bundle lands.
    pub(crate) lang: i32,
    /// Every committed chain block, newest first (the Chain-History panel).
    pub(crate) chain_rows: Vec<molt_core::ChainBlockView>,
    pub(crate) member: String,
    pub(crate) threshold_badge: String,
    pub(crate) surfaces: Vec<SurfaceData>,
    /// The chat sidebar's channel rows (chat bus).
    pub(crate) channels: Vec<ChannelRowData>,
    /// Canonical key of the selected channel (echoed into the UI so the
    /// sidebar highlight always matches what the engine filtered by).
    pub(crate) selected_key: String,
    /// Compose-banner label of the selected channel ("" = group).
    pub(crate) selected_label: String,
    /// The selected channel is a decided vote's read-only discussion
    /// (collapses the compose row, shows the banner's 🔒 note).
    pub(crate) selected_closed: bool,
    /// The selected channel is an ORGANIZATION decision's discussion — the
    /// compact detail panel above the log, and the nav section that stays
    /// expanded behind it ([`selected_channel_org`]).
    pub(crate) selected_org: bool,
    /// That decision's row, for the panel. Default when none is selected.
    pub(crate) selected_decision: ProposalRowData,
    /// Organization → Members table rows (engine `ReadMembers`), already
    /// ordered by the active sort.
    pub(crate) members: Vec<MemberRowData>,
    /// Organization → Uploads table rows (engine `ReadUploads`), already
    /// thinned by the filter and ordered by the active sort.
    pub(crate) uploads: Vec<UploadRowData>,
    /// Members sort echo: active column ("" = roster order) + direction —
    /// the headers render the ▲/▼ from these.
    pub(crate) members_sort: String,
    pub(crate) members_asc: bool,
    /// Uploads sort echo (like `members_sort`).
    pub(crate) uploads_sort: String,
    pub(crate) uploads_asc: bool,
    /// Uploads filter echo — lands in the filter box only when it differs
    /// (a workspace-switch reset or the members-table uploads-jump; live
    /// typing is guarded by the generation).
    pub(crate) uploads_filter: String,
    /// Effective (push-clamped) 0-based page per paged proposal-outcome
    /// list, keyed `"{surface}:{list}"` — `apply_surfaces` slices the
    /// declined/applied models with it and echoes "page x of y" into the
    /// surface tab (see [`ChatUiState::list_pages`]).
    pub(crate) list_pages: HashMap<String, usize>,
    /// The status info strip (founding date + mock activity trio).
    pub(crate) org_stats: OrgStats,
    /// Group-channel unread count (badges the Gruppe nav row).
    pub(crate) group_unread: i32,
}

/// The Organization → Status info strip, from the engine's Status reply.
pub(crate) struct OrgStats {
    /// Rendered founding date, always `YYYY-MM-DD` (a workspace without a
    /// recorded date shows the epoch, `1970-01-01`).
    pub(crate) founded: String,
    /// The republic's current image (engine `StatusView.image`): the
    /// materialized logo file inside the workspace directory (the bytes
    /// rode the applied proposal, so every device holds them).
    pub(crate) image: String,
    /// The effective "delete chat after" window (engine
    /// `StatusView.chat_retention_days`).
    pub(crate) retention_days: i32,
    /// Whether the open workspace is a chain-governed republic (engine
    /// `StatusView.chain_governed`) — the per-member "recovery link" action
    /// exists exactly there, so the Members table offers it only then.
    pub(crate) chain_governed: bool,
    /// The GROUP's relay pool (engine `StatusView.relays`) — a group setting
    /// shown beside the name and the retention window. Empty on a legacy
    /// queue-shaped republic, which has no relays.
    pub(crate) relays: Vec<String>,
    /// The charter's EFFECTIVE feature set (engine `StatusView.features`):
    /// which optional surfaces get a nav row and read as active under
    /// Organization › charter.
    pub(crate) features: Vec<String>,
    /// Decoded picture bytes a member picture may still carry here (engine
    /// `StatusView.image_budget`) - what the fit before proposing aims at.
    pub(crate) image_budget: u64,
}

/// One rendered row of the Organization → Members table.
pub(crate) struct MemberRowData {
    pub(crate) name: String,
    /// Identity-key fingerprint ("" on unanchored/demo workspaces).
    pub(crate) id: String,
    /// Full anchored identity key, lowercase hex ("" unanchored).
    pub(crate) pk: String,
    /// Rendered "last seen" label (prose is presentation; the engine
    /// serves the numeric stamp).
    pub(crate) last: String,
    /// The real last-seen unix stamp (0 = never) — the sort key behind
    /// the rendered label.
    pub(crate) last_ts: u64,
    /// 0 = online, 1 = stale, 2 = offline/unreachable (aged from the
    /// real stamp engine-side).
    pub(crate) state: i32,
    pub(crate) uploads: i32,
    /// R4 relay-split marker, pre-built by the engine ("" = none).
    pub(crate) split: String,
    /// The LOCAL file the engine materialized for this seat's applied
    /// picture ("" = none).
    pub(crate) image: String,
    /// That file's [`avatar_cache_key`] - the stat runs in the gather
    /// pass, off the UI thread, so the row mapping only looks it up.
    pub(crate) image_key: String,
    /// The seat's applied description ("" = none).
    pub(crate) desc: String,
}

/// One rendered row of the Organization → Uploads table (labels are
/// pre-rendered here; the .slint side only displays).
pub(crate) struct UploadRowData {
    /// The carrying chat message id (hex) — what download-file takes.
    pub(crate) id: String,
    pub(crate) user: String,
    pub(crate) date: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) size: String,
    pub(crate) available: bool,
    /// Sharer reachable (a user-to-user transfer needs them online).
    pub(crate) online: bool,
    /// Shortened real sha256 for the cell (the full hex rides MCP).
    pub(crate) checksum: String,
    pub(crate) expires: String,
    /// Live download status label ("" = idle): "42 %" while moving,
    /// a check mark when done, a warning sign when failed.
    pub(crate) status: String,
    /// 0 idle · 1 running · 2 done · 3 failed (drives color + button).
    pub(crate) status_kind: i32,
    /// §5.5 raw availability word (relay-held / sharer-only / gone).
    pub(crate) availability: String,
    /// Share time (unix seconds) — the sort key behind the rendered `date`.
    pub(crate) ts: u64,
    /// Size in bytes — the sort key behind the rendered `size` label.
    pub(crate) bytes: u64,
    /// Link expiry (unix seconds) — the sort key behind `expires`.
    pub(crate) expires_ts: u64,
    /// The FULL sha256 hex ("" on legacy shares) — the filter/sort key
    /// behind the shortened `checksum` cell, so a pasted full checksum
    /// still finds its row.
    pub(crate) checksum_full: String,
}

/// One chat-channel sidebar row (plain, `Send` twin of the Slint
/// `ChannelItem`). The group row's `label` stays empty — the UI substitutes
/// the localized `Strings.ch-group`.
pub(crate) struct ChannelRowData {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) icon: String,
    pub(crate) unread: i32,
}

/// UI-local chat-bus state shared between the Slint callbacks and the
/// mirror task. The SELECTED channel deliberately lives here (UI-local,
/// like `nav-collapsed`) and NOT in the shared `SessionView`: the filter
/// itself runs engine-side (`ReadState { channel }`), so GUI and MCP stay
/// co-equal — each operator passes its own filter, and which channel this
/// window looks at is presentation, not shared state.
#[derive(Default)]
pub(crate) struct ChatUiState {
    /// The relays the create wizard offers — this node's dialable set, as
    /// last pushed to the picker.
    pub(crate) create_relays: Vec<String>,
    /// Relays the founder DESELECTED there. Stored as the exclusion set, not
    /// the selection, so a relay confirmed after the wizard opened is
    /// included by default — the picker narrows a set, it does not freeze one.
    pub(crate) create_relays_off: std::collections::BTreeSet<String>,

    /// The workspace id this state belongs to — a switch resets everything
    /// (see [`ChatUiState::enter_workspace`]).
    pub(crate) workspace: String,
    /// The channel the chat pane shows; compose files new messages here.
    pub(crate) selected: ChannelRef,
    /// Proposal id → unix time this UI first saw it. Proposals carry no
    /// timestamp, so the patch-channel system lines interleave at this
    /// first-seen approximation (documented in `patch_system_lines`).
    pub(crate) first_seen: HashMap<u64, u64>,
    /// Everything this UI ever learned about a proposal from `pending` —
    /// the read contract's `pending` is Proposed-only, so a sealed/closed
    /// proposal vanishes from every read and only this cache keeps its
    /// patch channel titled and stated (see [`update_known_proposals`]).
    pub(crate) proposals: HashMap<u64, KnownProposal>,
    /// Push/selection generation. Concurrent `push_surfaces` runs race
    /// last-write-wins on the Slint event loop; every selection change and
    /// every push start bumps this, and a push whose captured generation
    /// is no longer current must neither apply its bundle nor touch the
    /// bundle apply (see [`ChatUiState::begin_push`]).
    pub(crate) generation: u64,
    /// Organization → Members sort: active column ("" = roster order).
    /// Like the channel selection this is UI-LOCAL presentation state —
    /// the engine's `ReadMembers`/`ReadUploads` stay the full projections
    /// (MCP sees them unchanged); this window merely re-orders/thins the
    /// mirrored rows before pushing them into the Slint models. A
    /// workspace switch resets it with the rest of this state.
    pub(crate) members_sort: String,
    /// Members sort direction (meaningful only while `members_sort` != "").
    pub(crate) members_asc: bool,
    /// Organization → Uploads sort: active column ("" = engine order).
    pub(crate) uploads_sort: String,
    /// Uploads sort direction (meaningful only while `uploads_sort` != "").
    pub(crate) uploads_asc: bool,
    /// Uploads filter needle: case-insensitive substring across user,
    /// filename and (full) checksum; "" = all rows.
    pub(crate) uploads_filter: String,
    /// Current 0-based page of the paged proposal-outcome lists, keyed
    /// `"{surface}:{list}"` (list = "declined" | "applied"); a missing key
    /// is page 0. UI-LOCAL presentation like the sorts — the engine's
    /// reads stay the full projections (MCP sees them unchanged). The
    /// stored page re-bases against the list's current length on every
    /// push ([`ChatUiState::clamp_list_page`]); a workspace switch resets
    /// it with the rest of this state.
    pub(crate) list_pages: HashMap<String, usize>,
}

impl ChatUiState {
    /// Bind the state to the active workspace. On a SWITCH (different id,
    /// including to/from "no workspace") everything resets: a stale
    /// Patch/Topic selection from the previous workspace must not filter
    /// the new one's log, and
    /// the first-seen stamps + proposal cache belong to the old
    /// proposals. Same id → no-op.
    pub(crate) fn enter_workspace(&mut self, active: &str) {
        if self.workspace != active {
            *self = ChatUiState {
                workspace: active.to_string(),
                // the epoch MOVES with the switch: a push read against the
                // previous workspace's log must not land on this one
                generation: self.generation + 1,
                ..ChatUiState::default()
            };
        }
    }

    /// Start one `push_surfaces` pass: take the current SELECTION EPOCH, or
    /// `None` when this push is reading for a workspace that is no longer the
    /// active one.
    ///
    /// It does NOT switch workspaces — [`ChatUiState::enter_workspace`] does,
    /// from the session mirror, which is ordered by the event stream. Letting
    /// each push switch on its own session copy meant a push that read the
    /// session before an open could re-enter the state as "no workspace"
    /// after it, discard the good push's bundle and land its own empty one:
    /// the empty chat on a first open.
    ///
    /// It deliberately does NOT bump. It used to, so that concurrent pushes
    /// resolved newest-wins — and that starved the chat pane: `push_surfaces`
    /// issues `MarkChannelRead` when the channel on screen has unread
    /// messages, the engine event that causes starts the next push, and the
    /// push already reading then threw its finished bundle away. Opening a
    /// chat with anything unread hit that every time: the pane stayed empty
    /// until some later burst of events happened to leave one push
    /// unoverlapped.
    ///
    /// Two pushes for the SAME selection read the same engine state and
    /// build the same bundle, so letting both land costs nothing. What must
    /// never land is a bundle read for a selection (or workspace) the user
    /// has since left — and that is exactly what the epoch tracks.
    pub(crate) fn begin_push(&self, active: &str) -> Option<u64> {
        (self.workspace == active).then_some(self.generation)
    }

    /// Select a channel. The bump invalidates every in-flight push: a
    /// bundle read for the previous selection must not land on — or mark
    /// read — the fresh one.
    pub(crate) fn select(&mut self, channel: ChannelRef) {
        self.selected = channel;
        self.generation += 1;
    }

    /// Whether the push stamped `gen` still describes the selection on
    /// screen; a stale push skips its apply closure.
    pub(crate) fn is_current(&self, gen: u64) -> bool {
        self.generation == gen
    }

    /// Click on a Members header column: toggle-or-switch the sort. The
    /// generation bump stales every in-flight push (its bundle carries the
    /// previous order).
    pub(crate) fn sort_members_by(&mut self, column: &str) {
        toggle_sort(&mut self.members_sort, &mut self.members_asc, column);
        self.generation += 1;
    }

    /// Click on an Uploads header column: toggle-or-switch the sort.
    pub(crate) fn sort_uploads_by(&mut self, column: &str) {
        toggle_sort(&mut self.uploads_sort, &mut self.uploads_asc, column);
        self.generation += 1;
    }

    /// The founder's pick as URLs — filled by the caller from the dialable
    /// set minus the deselected ones. Empty = no explicit choice.
    pub(crate) fn create_pick(&self) -> Vec<String> {
        if self.create_relays_off.is_empty() {
            return Vec::new();
        }
        self.create_relays
            .iter()
            .filter(|u| !self.create_relays_off.contains(*u))
            .cloned()
            .collect()
    }

    /// The picker's rows: every dialable relay, with whether it is chosen.
    pub(crate) fn create_pick_rows(&self) -> Vec<(String, bool)> {
        self.create_relays
            .iter()
            .map(|u| (u.clone(), !self.create_relays_off.contains(u)))
            .collect()
    }

    /// Refresh the offered set from the session's dialable pool, dropping any
    /// exclusion for a relay that is no longer offered.
    ///
    /// **No epoch bump**, and that is the fix for the empty chat on a first
    /// open. The epoch is the SELECTION epoch: it stales a surfaces bundle
    /// that was read for a channel or workspace the user has since left.
    /// The create wizard's relay picker is not in that bundle — the session
    /// mirror applies it directly — but it used to bump anyway, and opening
    /// a workspace changes the dialable pool. So the open's own bundle, nine
    /// chat rows and all, died between being gathered and being applied.
    /// Only on the FIRST open, because the pool only changes once, which is
    /// exactly how the user described it.
    pub(crate) fn set_create_relays(&mut self, dialable: Vec<String>) {
        if self.create_relays == dialable {
            return;
        }
        self.create_relays_off.retain(|u| dialable.contains(u));
        self.create_relays = dialable;
    }

    /// Flip one relay's pick in the create wizard. No epoch bump either, for
    /// the same reason: the picker is applied by the session mirror, not
    /// carried by a surfaces bundle.
    pub(crate) fn toggle_create_relay(&mut self, url: String) {
        if !self.create_relays_off.remove(&url) {
            self.create_relays_off.insert(url);
        }
    }

    /// Set the uploads filter needle (typed, or pre-filled by the Members
    /// table's uploads-jump).
    pub(crate) fn set_uploads_filter(&mut self, needle: String) {
        self.uploads_filter = needle;
        self.generation += 1;
    }

    /// Step a paged proposal-outcome list by `delta` pages (the pager's
    /// prev/next). Below the first page clamps at zero; the upper bound is
    /// enforced at push time ([`ChatUiState::clamp_list_page`] — only the
    /// push knows the list's current length). The generation bump stales
    /// every in-flight push (its bundle carries the previous page).
    pub(crate) fn page_list_by(&mut self, surface: &str, list: &str, delta: i32) {
        let page = self.list_pages.entry(format!("{surface}:{list}")).or_insert(0);
        *page = page.saturating_add_signed(delta as isize);
        self.generation += 1;
    }

    /// Re-base a stored page against the list's CURRENT length and return
    /// the effective 0-based page. The clamp writes back, so the next
    /// prev/next steps from the page the user actually sees — not from a
    /// stale out-of-range value a shrunk list left behind.
    pub(crate) fn clamp_list_page(&mut self, surface: &str, list: &str, len: usize) -> usize {
        let key = format!("{surface}:{list}");
        let stored = self.list_pages.get(&key).copied().unwrap_or(0);
        let (_, _, page, _) = page_slice(len, stored, LIST_PAGE_SIZE);
        if page != stored {
            self.list_pages.insert(key, page);
        }
        page
    }
}
pub(crate) struct SurfaceData {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) gated: bool,
    pub(crate) log: Vec<LogLineData>,
    pub(crate) pending: Vec<ProposalRowData>,
    /// Pending proposals this node already approved (the rest of `pending`
    /// still wait on this node's vote — the approvals table shows the split).
    pub(crate) pending_voted: usize,
    /// Declined proposals against this surface (total, for the status strip).
    pub(crate) denied: usize,
    /// The declined proposals still inside the display-retention window —
    /// the Declined view empties on the same rhythm as the group chat.
    pub(crate) declined: Vec<ProposalRowData>,
    /// The applied history as decision rows (newest first): the Accepted
    /// table — proposal-backed rows carry their sealed block's voters,
    /// legacy rows (id -1) only their title. Parallel to the gated `log`.
    pub(crate) accepted: Vec<ProposalRowData>,
    /// Memory only: the engine-folded wiki base + its revision (the
    /// shared truth the Wiki model rebases on — shared_memory_real.md).
    pub(crate) wiki_tree: Vec<(String, String)>,
    pub(crate) wiki_rev: u64,
}
pub(crate) struct LogLineData {
    /// Stable message id, 32-char hex ("" on legacy entries without one —
    /// such rows must never offer id-requiring actions, see the `id != ""`
    /// guards in the .slint files: the UI must not fake success).
    pub(crate) id: String,
    pub(crate) lead: String,
    pub(crate) text: String,
    pub(crate) when: String,
    pub(crate) quote: i32,
    /// Quoted message by stable id ("" = none / legacy numeric quote).
    pub(crate) quote_id: String,
    /// A UI-synthesized governance line (patch channels, P8): quiet
    /// styling, no author, no actions.
    pub(crate) system: bool,
    pub(crate) quote_label: String,
    /// Reply-indent depth (0 = no quote; 1/2 alternate between NEIGHBORING
    /// quote groups of different targets, so stacked replies to different
    /// questions stop reading as one thread) — annotate_chat_log fills it.
    pub(crate) quote_indent: i32,
    pub(crate) deleted_by: String,
    pub(crate) first: bool,
    pub(crate) own: bool,
    pub(crate) alt: bool,
    pub(crate) mine_emoji: String,
    pub(crate) reactions: Vec<ReactionData>,
    /// Read receipts, shown ONLY on the local member's OWN messages (the
    /// sender wants to know it arrived): one dot per other member, green once
    /// they have read it, yellow until then. Empty on incoming messages,
    /// legacy/system rows, and tombstones. Display is additionally gated on
    /// the local read-receipts switch in the .slint (symmetric hide).
    pub(crate) receipts: Vec<ReceiptData>,
    pub(crate) has_file: bool,
    pub(crate) file_name: String,
    pub(crate) file_meta: String,
    pub(crate) file_available: bool,
    /// The proposal this applied-log row came from (the snapshot's parallel
    /// id track) — the 💬 jump into its discussion channel. `None` on chat
    /// rows, system lines and rows of unknown origin (legacy dumps): those
    /// must offer no jump (feedback honesty, like the `id != ""` guards).
    pub(crate) proposal_id: Option<u64>,
}
pub(crate) struct ReactionData {
    pub(crate) emoji: String,
    pub(crate) count: i32,
    pub(crate) mine: bool,
}
pub(crate) struct ReceiptData {
    /// The member this dot represents.
    pub(crate) name: String,
    /// Whether they have confirmed reading (green) or not yet (yellow).
    pub(crate) read: bool,
}
/// `Default` = "no decision selected": the compact panel above a chat needs
/// an empty row when the selected channel is not a decision at all, and the
/// panel itself is gated on `selected_org` so the empty one never renders.
#[derive(Default)]
pub(crate) struct ProposalRowData {
    pub(crate) id: i32,
    pub(crate) text: String,
    pub(crate) approvals: i32,
    pub(crate) threshold: i32,
    /// Ist-Stand / Soll-Stand display pair ("" = hidden line).
    pub(crate) current: String,
    pub(crate) proposed: String,
    /// set_image / remove_image: the card renders the current picture and
    /// links the proposed image (its bytes ride the payload).
    pub(crate) image_op: bool,
    /// A pending set_image's embedded bytes (base64; "" otherwise) — the
    /// preview decodes them locally on every member's device.
    pub(crate) img_b64: String,
    /// set_charter: long Ist/Soll texts render capped + scrollable.
    pub(crate) charter_op: bool,
    /// wiki_patch: the proposed value IS a raw git patch — monospace,
    /// capped + scrollable.
    pub(crate) patch_op: bool,
    /// set_relays: the vote card renders the pool DIFF instead of the
    /// generic Ist/Soll pair — one row per union member, marked
    /// kept/added/removed (`RELAY_ROW_*`). Empty = not a relay op.
    pub(crate) relay_changes: Vec<(i32, String)>,
    /// Per-member stance in roster order (0 open · 1 approved · 2 declined).
    pub(crate) votes: Vec<(String, i32)>,
    /// Who declined it ("" = not declined) + the human "when" label.
    pub(crate) declined_by: String,
    pub(crate) declined_when: String,
    /// The READING member's own stance (0 open · 1 approved · 2 declined):
    /// a cast stance grays the vote buttons — clickable OR grayed, never
    /// click-then-refusal (story 2026-08-09).
    pub(crate) my_vote: i32,
    /// Whether the READING member proposed it (engine `ProposalView.mine`)
    /// — the "pull back" button's visibility gate.
    pub(crate) mine: bool,
    /// The supersede walk retired it (base moved) — labeled "superseded",
    /// never "declined by" (no vote was cast).
    pub(crate) superseded: bool,
    /// The vote ended by APPLYING — an applied patch's changes live in
    /// the base, so the card never offers the rescue.
    pub(crate) applied: bool,
    /// The proposer pulled it back — "pulled back", never "declined by".
    pub(crate) withdrawn: bool,
    /// Unread messages in this proposal's discussion channel (the 💬
    /// button's badge); 0 = caught up.
    pub(crate) unread: i32,
}

/// Read status + every surface snapshot into a bundle the window can apply.
///
/// `None` = nothing to render: no session, or this pass is reading for a
/// workspace/selection the user has since left.
///
/// The chat surface is read TWICE: once unfiltered (channel enumeration and
/// quote teasers are whole-log concerns — a quote may point across
/// channels), and once through the engine's channel filter for the
/// displayed log. Filtering client-side would break co-equality with MCP,
/// so the filter deliberately rides `ReadState { channel }`.
///
/// Deliberately free of the window: every decision this layer makes lives
/// here, so a test can make them without a display (`gui_tests`).
pub(crate) async fn gather_surfaces(
    wallet: &WalletHandle,
    chat_ui: &Arc<Mutex<ChatUiState>>,
) -> Option<(u64, SurfacesBundle)> {
    let (member, threshold_badge, org_stats) = match wallet.execute(Command::Status).await {
        Ok(Reply::Status(s)) => (
            s.member,
            format!("{}-of-{}", s.threshold, s.members.len()),
            OrgStats {
                // the literal epoch (not file_date_label(0): a negative-UTC
                // timezone would render ts 0 as 1969-12-31)
                founded: if s.founded_ts == 0 {
                    "1970-01-01".to_string()
                } else {
                    file_date_label(s.founded_ts)
                },
                image: s.image,
                retention_days: i32::try_from(s.chat_retention_days).unwrap_or(7),
                chain_governed: s.chain_governed,
                relays: s.relays,
                features: s.features,
                image_budget: s.image_budget,
            },
        ),
        _ => return None,
    };
    // the chat-bus UI state is per-workspace: bind it to the active id so
    // a workspace switch drops the previous selection/unread/first-seen
    // (the language rides along — a SetLanguage emits a Full session
    // change, which re-runs this push, so the nav labels stay live)
    // The chat is ONE window (`Surface::Chat` has a single view), so the GUI
    // never narrows the read: `ReadState { view: None }` IS the General
    // pane. The engine's `view` axis survives for the agent-facing "unread"
    // slice, which is deliberately not somewhere a human navigates.
    let (active_ws, lang, mark_read_active) = match wallet.execute(Command::ReadSession).await {
        Ok(Reply::Session(s)) => (
            s.active_workspace.clone(),
            i32::from(s.language == "de"),
            // only auto-confirm reads when the chat surface is on screen AND
            // this node's read receipts are enabled (off = reveal nothing, so
            // do not even issue the no-op'd command)
            s.surface == Surface::Chat && s.settings.read_receipts,
        ),
        _ => (String::new(), 0, false),
    };
    // stamp this push BEFORE the surface reads: any selection change or
    // newer push from here on makes this pass stale, and a stale pass must
    // not land its bundle (concurrent pushes
    // otherwise race last-write-wins and can revert a fresh selection)
    let Some((my_gen, selected)) = chat_ui
        .lock()
        .ok()
        .and_then(|s| Some((s.begin_push(&active_ws)?, s.selected.clone())))
    else {
        // this push read the session for a workspace that is no longer the
        // active one — its bundle would describe the wrong log
        return None;
    };
    let full_chat = match wallet
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            // whole-log concerns (channel enumeration, quote teasers):
            // deliberately no view filter — a quote may point across the
            // today/archive boundary like it may across channels
            view: None,
        })
        .await
    {
        Ok(Reply::State(snap)) => Some(snap),
        _ => None,
    };
    // the Organization tables ride the same push: the engine's ReadMembers /
    // ReadUploads (the projections the MCP tools of the same name read)
    let members: Vec<MemberRowData> = match wallet.execute(Command::ReadMembers).await {
        Ok(Reply::Members { members: rows }) => rows
            .into_iter()
            .map(|m| MemberRowData {
                name: m.member,
                id: m.id,
                pk: m.identity_pk,
                last: seen_label(lang, unix_now(), m.last_seen, never_seen_label(lang)),
                last_ts: m.last_seen,
                state: i32::from(m.presence),
                uploads: i32::try_from(m.uploads).unwrap_or(i32::MAX),
                split: m.split,
                image_key: avatar_cache_key(&m.image),
                image: m.image,
                desc: m.description,
            })
            .collect(),
        _ => Vec::new(),
    };
    let upload_now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let uploads: Vec<UploadRowData> = match wallet.execute(Command::ReadUploads).await {
        Ok(Reply::Uploads { uploads: rows }) => rows
            .into_iter()
            .map(|u| UploadRowData {
                id: u.id.to_string(),
                user: u.member,
                date: file_date_label(u.ts),
                name: u.name,
                kind: u.kind,
                size: file_size_label(u.size),
                available: u.available,
                online: u.online,
                checksum: u
                    .checksum
                    .get(..10)
                    .map(|s| format!("{s}…"))
                    .unwrap_or_default(),
                expires: expires_label(lang, upload_now, u.expires_ts, u.available),
                ts: u.ts,
                bytes: u.size,
                expires_ts: u.expires_ts,
                availability: u.availability,
                checksum_full: u.checksum,
                status: match u.download.as_ref().map(|d| d.phase.as_str()) {
                    Some("requested") => "0 %".to_string(),
                    Some("transferring") => u
                        .download
                        .as_ref()
                        .map(|d| format!("{} %", d.percent))
                        .unwrap_or_default(),
                    Some("done") => "\u{2713}".to_string(),
                    Some("failed") => "\u{26a0}".to_string(),
                    _ => String::new(),
                },
                status_kind: match u.download.as_ref().map(|d| d.phase.as_str()) {
                    Some("requested" | "transferring") => 1,
                    Some("done") => 2,
                    Some("failed") => 3,
                    _ => 0,
                },
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut snaps: Vec<(Surface, SurfaceSnapshot)> = Vec::new();
    for sf in Surface::ALL {
        // charter feature gating: a disabled optional surface gets NO nav
        // row (hidden, not greyed) — the engine-side twin refuses selecting
        // it, so hidden and refused are one verdict
        if sf.is_charter_feature()
            && !org_stats.features.iter().any(|f| f == sf.as_str())
        {
            continue;
        }
        let channel = (sf == Surface::Chat).then(|| selected.clone());
        // the displayed chat log follows the selected sub-view: General
        // shows the younger half of the retention window, Archive the
        // older half — filtered engine-side, same as the channel
        let view: Option<String> = None;
        if let Ok(Reply::State(snap)) = wallet
            .execute(Command::ReadState { surface: sf, channel, view })
            .await
        {
            snaps.push((sf, snap));
        }
    }
    // D2 read-receipts trigger: while the chat surface is the one on screen,
    // confirm the loaded messages of the selected channel as read — every
    // message not mine, live, human, with a real id, and not already read by
    // me. One batched MarkRead; the engine no-ops it when read receipts are
    // disabled locally or nothing is fresh, so firing on every chat refresh is
    // safe and idempotent (a repeat filters to empty → no re-broadcast).
    if mark_read_active {
        if let Some((_, chat_snap)) = snaps.iter().find(|(sf, _)| *sf == Surface::Chat) {
            let fresh: Vec<molt_core::MessageId> = chat_messages(chat_snap)
                .into_iter()
                .filter(|m| {
                    !m.id.is_nil()
                        && m.kind.is_user()
                        && m.deleted_by.is_none()
                        && m.from != member
                        && !m.read_by.contains(&member)
                })
                .map(|m| m.id)
                .collect();
            if !fresh.is_empty() {
                let _ = wallet.execute(Command::MarkRead { ids: fresh }).await;
            }
        }
    }
    // proposal state across ALL surfaces feeds the patch channels: lazy
    // titles for the sidebar and the system lines (P8)
    let all_pending: Vec<ProposalView> = snaps
        .iter()
        .flat_map(|(_, s)| s.pending.iter().cloned())
        .collect();
    // …and the declined lists feed the cache too: a veto this UI never saw
    // pending (fresh open, other member's decline) must still title its
    // discussion channel and flag it closed
    let all_declined: Vec<ProposalView> = snaps
        .iter()
        .flat_map(|(_, s)| s.declined.iter().cloned())
        .collect();
    // the gated surfaces' applied logs — the proposal cache resolves a
    // vanished proposal's fate against them (the applied values ARE the
    // raw proposal payloads, for the chain and the legacy path alike)
    let applied_by_surface: HashMap<Surface, Vec<serde_json::Value>> = snaps
        .iter()
        .filter(|(sf, _)| *sf != Surface::Chat)
        .map(|(sf, s)| (*sf, s.applied.clone()))
        .collect();
    let full_msgs = full_chat.as_ref().map(chat_messages).unwrap_or_default();
    // the engine enumerates the channels (P7): every distinct ref in the
    // log, `Group` always present — authoritative for the chat surface
    // (empty only when no chat read succeeded, i.e. nothing is open)
    let infos = full_chat
        .as_ref()
        .map(|s| s.channels.clone())
        .unwrap_or_default();
    let quotes = quote_sources(&full_msgs);
    let selected_key = channel_key(&selected);
    // B2: unread comes from the ENGINE's per-channel cursor now (the same
    // count an MCP agent reads) — the on-screen channel renders 0 and its
    // cursor is advanced below, because being on screen IS being read
    let engine_unread: HashMap<String, usize> = infos
        .iter()
        .map(|i| {
            let key = channel_key(&i.channel);
            let u = if key == selected_key { 0 } else { i.unread };
            (key, u)
        })
        .collect();
    if full_chat.is_some()
        && infos
            .iter()
            .any(|i| channel_key(&i.channel) == selected_key && i.unread > 0)
    {
        let _ = wallet
            .execute(Command::MarkChannelRead { channel: selected.clone(), up_to: String::new() })
            .await;
    }
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    // the Chain-History settings panel: every committed block, newest
    // first (co-equal read — the MCP read_chain tool serves the same)
    let chain_rows: Vec<molt_core::ChainBlockView> = match wallet.execute(Command::ReadChain).await
    {
        Ok(Reply::Chain { blocks }) => blocks,
        _ => Vec::new(),
    };
    let chain_len = chain_rows.len();
    let (unread, first_seen, known, org_view, list_pages) = {
        // …and here for the same reason: this is the chat pane's own
        // bookkeeping, and losing the mirror is worse than working from a
        // state some other panic left mid-update
        let mut st = match chat_ui.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !st.is_current(my_gen) {
            // a newer selection/push owns the state — observing now would
            // mis-mark the fresh channel read, and the bundle is stale
            return None;
        }
        for p in &all_pending {
            st.first_seen.entry(p.id.0).or_insert(now);
        }
        update_known_proposals(&mut st.proposals, &all_pending, &all_declined, &applied_by_surface);
        // the paged proposal-outcome lists: re-base every stored page
        // against its list's CURRENT length (a shrunk list must never
        // leave the view on a page that no longer exists), then capture
        // the effective pages for the bundle. Chat's log is the chat
        // pane — never paged here.
        let mut list_pages: HashMap<String, usize> = HashMap::new();
        for (sf, s) in &snaps {
            if *sf == Surface::Chat {
                continue; // the chat log IS the pane — full scrollback, never paged
            }
            let key = sf.as_str();
            list_pages.insert(
                format!("{key}:declined"),
                st.clamp_list_page(key, "declined", s.declined.len()),
            );
            list_pages.insert(
                format!("{key}:applied"),
                st.clamp_list_page(key, "applied", s.applied.len()),
            );
        }
        list_pages.insert(
            "chain:history".to_string(),
            st.clamp_list_page("chain", "history", chain_len),
        );
        (
            engine_unread,
            st.first_seen.clone(),
            st.proposals.clone(),
            (
                st.members_sort.clone(),
                st.members_asc,
                st.uploads_sort.clone(),
                st.uploads_asc,
                st.uploads_filter.clone(),
            ),
            list_pages,
        )
    };
    // the Organization tables' presentation pass (UI-local, like the
    // channel selection): thin the uploads by the filter needle, then
    // order both tables by their active sort column — the engine's
    // ReadMembers/ReadUploads projections stay the full, untouched truth
    let (members_sort, members_asc, uploads_sort, uploads_asc, uploads_filter) = org_view;
    let mut members = members;
    sort_members(&mut members, &members_sort, members_asc);
    let mut uploads = filter_uploads(uploads, &uploads_filter);
    sort_uploads(&mut uploads, &uploads_sort, uploads_asc);
    // titles come from the cache, so a patch channel keeps its name (and
    // its ✓/⊘ state line) after the proposal left the Proposed-only read
    let titles = known_titles(lang, &known);
    let channels = derive_channels(lang, &infos, &known, &unread);
    // the group channel has no sidebar row anymore — its unread count
    // badges the Gruppe nav row instead
    let group_unread =
        i32::try_from(unread.get("group").copied().unwrap_or(0)).unwrap_or(i32::MAX);
    let selected_label = channel_display_label(&selected, &titles);
    let selected_closed = selected_channel_closed(&selected, &infos, &known);
    let selected_org = selected_channel_org(&selected, &known);
    // the panel renders from the SAME projection the Organization pane uses,
    // so the two can never drift apart. A DECIDED vote is in neither the
    // pending nor the declined read, but its discussion stays a selectable
    // read-only view — the header must carry the decided card, so the
    // lookup falls back to the full proposal list (an empty default card
    // here was the reported half-page "Proposal:" wreck, 2026-08-09)
    let selected_decision = match &selected {
        ChannelRef::Patch { id } => {
            let live = all_pending
                .iter()
                .chain(all_declined.iter())
                .find(|p| p.id == *id)
                .map(|p| proposal_row(lang, p));
            match live {
                Some(row) => row,
                None => match wallet.execute(Command::ListProposals).await {
                    Ok(Reply::Proposals { proposals }) => proposals
                        .iter()
                        .find(|p| p.id == *id)
                        .map(|p| proposal_row(lang, p))
                        .unwrap_or_default(),
                    _ => ProposalRowData::default(),
                },
            }
        }
        _ => ProposalRowData::default(),
    };
    let ctx = ChatViewCtx {
        selected,
        proposals: all_pending,
        known,
        first_seen,
        quotes,
        roster: members.iter().map(|m| m.name.clone()).collect(),
    };
    let surfaces: Vec<SurfaceData> = snaps
        .iter()
        .map(|(sf, snap)| {
            surface_data(
                lang,
                *sf,
                snap,
                &member,
                (*sf == Surface::Chat).then_some(&ctx),
                &unread,
            )
        })
        .collect();
    let bundle = SurfacesBundle {
        lang,
        chain_rows,
        member,
        threshold_badge,
        surfaces,
        channels,
        selected_key,
        selected_label,
        selected_closed,
        selected_org,
        selected_decision,
        members,
        uploads,
        members_sort,
        members_asc,
        uploads_sort,
        uploads_asc,
        uploads_filter,
        list_pages,
        org_stats,
        group_unread,
    };
    tracing::debug!(
        ws = %active_ws,
        gen = my_gen,
        channel = %bundle.selected_key,
        chat_rows = bundle
            .surfaces
            .iter()
            .find(|s| s.key == "chat")
            .map_or(0, |s| s.log.len()),
        "ui: bundle gathered"
    );
    Some((my_gen, bundle))
}

/// Project one surface snapshot into plain display data. `me` is the local
/// member handle — it marks own messages and the own reaction pill.
/// `chat_ctx` is `Some` for the chat surface only.
pub(crate) fn surface_data(
    lang: i32,
    sf: Surface,
    snap: &SurfaceSnapshot,
    me: &str,
    chat_ctx: Option<&ChatViewCtx>,
    unread: &HashMap<String, usize>,
) -> SurfaceData {
    // the 💬 badge: unread entries in a proposal's discussion channel
    let badge = |row: ProposalRowData| -> ProposalRowData {
        let n = unread
            .get(&format!("patch:{}", row.id))
            .copied()
            .unwrap_or(0);
        ProposalRowData {
            unread: i32::try_from(n).unwrap_or(i32::MAX),
            ..row
        }
    };
    let mut log: Vec<LogLineData> = if sf == Surface::Chat {
        let msgs = chat_messages(snap);
        // the retention window ("delete chat after N days") is ENGINE
        // semantics now — the read already arrives filtered, identically
        // for the GUI and an MCP agent (co-equality)
        let roster = chat_ctx.map(|c| c.roster.as_slice()).unwrap_or(&[]);
        let pairs: Vec<(u64, LogLineData)> = msgs
            .iter()
            .map(|m| (m.ts, chat_line(lang, m, me, roster)))
            .collect();
        let system = match chat_ctx.map(|c| &c.selected) {
            Some(ChannelRef::Patch { id }) => {
                let ctx = chat_ctx.expect("checked above");
                patch_system_lines(lang, id.0, &ctx.proposals, &ctx.known, &ctx.first_seen)
            }
            _ => Vec::new(),
        };
        merge_by_time(pairs, system)
    } else {
        snap.applied
            .iter()
            .enumerate()
            .map(|(i, v)| LogLineData {
                id: String::new(),
                lead: String::new(),
                text: display_title(lang, v),
                when: String::new(),
                quote: -1,
                quote_id: String::new(),
                system: false,
                quote_label: String::new(),
                quote_indent: 0,
                deleted_by: String::new(),
                first: true,
                own: false,
                alt: false,
                mine_emoji: String::new(),
                reactions: Vec::new(),
                receipts: Vec::new(),
                has_file: false,
                file_name: String::new(),
                file_meta: String::new(),
                file_available: false,
                // the id track is positionally parallel to `applied`; a
                // pre-id peer's snapshot has an empty/short track → None
                proposal_id: snap.applied_ids.get(i).copied().flatten(),
            })
            .collect()
    };
    let no_quotes = HashMap::new();
    annotate_chat_log(&mut log, chat_ctx.map_or(&no_quotes, |c| &c.quotes));
    let pending: Vec<ProposalRowData> = snap
        .pending
        .iter()
        .map(|p| badge(proposal_row(lang, p)))
        .collect();
    // the Declined view empties on the chat-retention rhythm — engine
    // semantics too (the read arrives pre-filtered on declined_at)
    let declined: Vec<ProposalRowData> = snap
        .declined
        .iter()
        .map(|p| badge(proposal_row(lang, p)))
        .collect();
    // the Accepted table: the applied history in apply order (newest
    // first), each proposal-backed row carrying the voters its sealed
    // block proves; a row of unknown origin (legacy dump) still shows,
    // just without votes or a discussion jump. Gated surfaces only — a
    // chat surface's applied entries are its messages, never table rows.
    let accepted: Vec<ProposalRowData> = if snap.gated {
        let by_id: HashMap<u64, &molt_core::ProposalView> =
            snap.accepted.iter().map(|p| (p.id.0, p)).collect();
        snap.applied
            .iter()
            .enumerate()
            .rev()
            .map(|(i, payload)| {
                match snap
                    .applied_ids
                    .get(i)
                    .copied()
                    .flatten()
                    .and_then(|id| by_id.get(&id))
                {
                    Some(p) => proposal_row(lang, p),
                    None => ProposalRowData {
                        id: -1,
                        text: display_title(lang, payload),
                        // a table CELL: the compact summary when the payload
                        // carries one AS A STRING, else the value's first
                        // line — keyed on string-ness, not key presence (a
                        // foreign payload's `"summary": null` must fall
                        // through, not blank the cell), and never multi-line
                        // (a git patch dump burst the row)
                        proposed: payload
                            .get("summary")
                            .and_then(serde_json::Value::as_str)
                            .or_else(|| {
                                payload.get("value").and_then(serde_json::Value::as_str)
                            })
                            .unwrap_or_default()
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_string(),
                        ..ProposalRowData::default()
                    },
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    SurfaceData {
        key: sf.as_str().to_string(),
        name: surface_name(lang, sf).to_string(),
        gated: snap.gated,
        log,
        pending,
        pending_voted: snap.pending.iter().filter(|p| p.approved_by_me).count(),
        denied: snap.denied,
        declined,
        accepted,
        wiki_tree: snap
            .wiki_tree
            .iter()
            .map(|d| (d.path.clone(), d.content.clone()))
            .collect(),
        wiki_rev: snap.wiki_rev,
    }
}

/// Project one chain block into its Chain-History row — titles render in
/// the ACTIVE language from the payload's op placeholder, exactly like the
/// applied logs (language-neutral wire, localized display).
pub(crate) fn chain_row(lang: i32, r: &molt_core::ChainBlockView) -> ChainRow {
    let de = lang == 1;
    let (kind, title) = match r.kind.as_str() {
        "genesis" => (
            strings_pick(de, "Founding", "Gründung"),
            r.payload.as_str().unwrap_or_default().to_string(),
        ),
        "membership" => (
            strings_pick(de, "Membership", "Mitgliedschaft"),
            r.payload.as_str().unwrap_or_default().to_string(),
        ),
        "checkpoint" => (
            strings_pick(de, "Checkpoint (compacted)", "Checkpoint (kompaktiert)"),
            format!(
                "{} {}",
                strings_pick(de, "state up to block", "Zustand bis Block"),
                r.payload.as_u64().unwrap_or(0)
            ),
        ),
        // applied: the payload IS the proposal payload — op-placeholder title
        _ => (String::new(), display_title(lang, &r.payload)),
    };
    ChainRow {
        height: if r.height == 0 && r.kind == "applied" {
            strings_pick(de, "- (before the cut)", "- (vor dem Schnitt)")
        } else {
            format!("#{}", r.height)
        }
        .into(),
        kind: kind.into(),
        surface: if r.surface.is_empty() {
            String::new()
        } else {
            Surface::parse(&r.surface)
                .map(|sf| surface_name(lang, sf).to_string())
                .unwrap_or_else(|| r.surface.clone())
        }
        .into(),
        title: title.into(),
        signers: r.signers.join(", ").into(),
    }
}

/// Project one proposal view into the card row the GUI renders — shared by
/// the pending and the declined list.
/// The Slint projection of one [`ProposalRowData`].
///
/// A free function rather than a per-surface closure: the compact decision
/// panel above a chat renders the SAME row the Organization pane does, and
/// two conversions would be two things to keep in step.
/// One INDEX row of the decided-votes table (Accepted / Declined).
///
/// The same projection as the card's, with the value flattened to a single
/// line: a seat description is typed into a multi-line box, so its value can
/// carry newlines — rendered verbatim they make a 40px table row three lines
/// tall and paint over the neighbouring rows. The table is an index (one line
/// per decision); the full text lives behind the discussion jump.
pub(crate) fn to_decided_row(p: &ProposalRowData) -> ProposalRow {
    ProposalRow {
        current: one_line(&p.current).into(),
        proposed: one_line(&p.proposed).into(),
        ..to_proposal_row(p)
    }
}

/// Collapse every run of whitespace — newlines included — into one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn to_proposal_row(p: &ProposalRowData) -> ProposalRow {
    ProposalRow {
        id: p.id,
        text: p.text.clone().into(),
        approvals: p.approvals,
        threshold: p.threshold,
        current: p.current.clone().into(),
        proposed: p.proposed.clone().into(),
        image_op: p.image_op,
        img_b64: p.img_b64.as_str().into(),
        charter_op: p.charter_op,
        patch_op: p.patch_op,
        relay_op: !p.relay_changes.is_empty(),
        relay_changes: ModelRc::new(VecModel::from(
            p.relay_changes
                .iter()
                .map(|(sign, url)| RelayChange {
                    sign: *sign,
                    url: url.as_str().into(),
                })
                .collect::<Vec<_>>(),
        )),
        votes: ModelRc::new(VecModel::from(
            p.votes
                .iter()
                .map(|(member, vote)| MemberVoteMark {
                    member: member.as_str().into(),
                    vote: *vote,
                })
                .collect::<Vec<_>>(),
        )),
        declined_by: p.declined_by.clone().into(),
        declined_when: p.declined_when.clone().into(),
        my_vote: p.my_vote,
        mine: p.mine,
        superseded: p.superseded,
        applied: p.applied,
        withdrawn: p.withdrawn,
        unread: p.unread,
    }
}

/// Row markers of the set_relays vote card (`RelayChange.sign`).
pub(crate) const RELAY_ROW_KEPT: i32 = 0;
pub(crate) const RELAY_ROW_ADDED: i32 = 1;
pub(crate) const RELAY_ROW_REMOVED: i32 = 2;

/// The set_relays vote card's diff: both pools are space-separated URL
/// lists, the rows are the union — current pool in its own order marked
/// kept/removed, then the additions in proposal order. Duplicates
/// collapse; the strings stay verbatim (the engine canonicalizes at
/// propose). An EMPTY proposed pool yields no rows — the engine folds such
/// an edit as a no-op, so a diff promising removals would be a
/// sign-what-you-see lie; the card falls back to the generic pair.
///
/// Format coupling, deliberate: `proposed` is the op's wire format, but
/// `current` re-splits `change_summary`'s space-joined DISPLAY string
/// (`ProposalView.current`) — if that join ever changes shape, these rows
/// silently go bogus. The gui test pins the wiring.
pub(crate) fn relay_pool_diff(current: &str, proposed: &str) -> Vec<(i32, String)> {
    let cur: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        current
            .split_whitespace()
            .filter(|u| seen.insert(*u))
            .collect()
    };
    let new: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        proposed
            .split_whitespace()
            .filter(|u| seen.insert(*u))
            .collect()
    };
    if new.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<(i32, String)> = cur
        .iter()
        .map(|u| {
            let sign = if new.contains(u) {
                RELAY_ROW_KEPT
            } else {
                RELAY_ROW_REMOVED
            };
            (sign, (*u).to_string())
        })
        .collect();
    rows.extend(
        new.iter()
            .filter(|u| !cur.contains(*u))
            .map(|u| (RELAY_ROW_ADDED, (*u).to_string())),
    );
    rows
}

/// L10: the retention pair travels as a MACHINE value ("30"); the unit
/// renders here, in the active language — a legacy payload that still
/// carries "30 days" is normalized by taking its leading number.
pub(crate) fn retention_value(lang: i32, raw: &str) -> String {
    let n = raw.split_whitespace().next().unwrap_or(raw);
    if n.parse::<u64>().is_err() {
        return raw.to_string();
    }
    if lang == 1 {
        format!("{n} Tage")
    } else {
        format!("{n} days")
    }
}

pub(crate) fn proposal_row(lang: i32, p: &molt_core::ProposalView) -> ProposalRowData {
    let op = p
        .payload
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    ProposalRowData {
        id: p.id.0 as i32,
        text: display_title(lang, &p.payload),
        approvals: p.approvals as i32,
        threshold: p.threshold as i32,
        current: if op == "set_chat_retention" {
            retention_value(lang, &p.current)
        } else {
            p.current.clone()
        },
        proposed: if op == "set_chat_retention" {
            retention_value(lang, &p.proposed)
        } else {
            p.proposed.clone()
        },
        // a member picture rides the org logo's card: inline preview and
        // the save path, both driven off the payload's bytes
        image_op: matches!(
            op,
            "set_image" | "remove_image" | "set_member_image" | "remove_member_image"
        ),
        img_b64: p
            .payload
            .get("bytes_b64")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        charter_op: op == "set_charter",
        patch_op: op == "wiki_patch",
        relay_changes: match op {
            "set_relays" => relay_pool_diff(&p.current, &p.proposed),
            // the feature vote reuses the set-diff rows; keys render as
            // their display labels — one vocabulary with the nav and the
            // wizard. A "removed" verdict is remapped to KEPT: the union
            // fold can never remove, and `current` is recomputed live, so a
            // racing enable would otherwise paint an impossible red minus
            // on a governance card (review 2026-08-12).
            "set_features" => relay_pool_diff(&p.current, &p.proposed)
                .into_iter()
                .map(|(sign, key)| {
                    let label = Surface::parse(&key)
                        .map(|sf| surface_name(lang, sf).to_string())
                        .unwrap_or(key);
                    (if sign == RELAY_ROW_REMOVED { RELAY_ROW_KEPT } else { sign }, label)
                })
                .collect(),
            _ => Vec::new(),
        },
        votes: p
            .votes
            .iter()
            .map(|v| {
                let stance = match v.vote {
                    molt_core::VoteState::Open => 0,
                    molt_core::VoteState::Approved => 1,
                    molt_core::VoteState::Declined => 2,
                };
                (v.member.clone(), stance)
            })
            .collect(),
        declined_by: p.declined_by.clone(),
        declined_when: if p.declined_at > 0 {
            when_label(lang, p.declined_at)
        } else {
            String::new()
        },
        my_vote: if p.declined_by_me {
            2
        } else if p.approved_by_me {
            1
        } else {
            0
        },
        mine: p.mine,
        superseded: p.superseded,
        applied: p.state == molt_core::ProposalState::Applied,
        withdrawn: p.withdrawn,
        unread: 0, // filled by the caller where the unread map is at hand
    }
}

/// A short human label for a surface transition payload: the human title
/// alone — the op code stays wire-side (nobody proposes "set_image", they
/// propose "Logo"). The op is only the fallback when a payload (e.g. a
/// minimal MCP proposal) carries no display key at all.
pub(crate) fn summarize(v: &serde_json::Value) -> String {
    if let Some(obj) = v.as_object() {
        for key in ["title", "label", "memo", "note", "text", "name", "summary"] {
            if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
                return s.to_string();
            }
        }
        if let Some(o) = obj.get("op").and_then(serde_json::Value::as_str) {
            return o.to_string();
        }
    }
    v.to_string()
}

/// The localized label of an Organization governance op (`None` = not a
/// governance op). These live HERE, not in the payload: the payload carries
/// only the machine `op` placeholder, so every UI renders the title in its
/// own active language (a pre-rendered string would freeze the proposer's
/// language and mix languages across the group).
fn org_op_label(lang: i32, op: &str) -> Option<&'static str> {
    Some(match (lang, op) {
        (1, "set_name") => "Name ändern",
        (_, "set_name") => "Rename",
        // short noun labels, no leading "Change …"/"… ändern" verb: a
        // proposal is a change by definition, and the sidebar channel
        // list elides anything long
        (1, "set_charter") => "Satzung",
        (_, "set_charter") => "Charter",
        (1, "set_image") => "Logo",
        (_, "set_image") => "Logo",
        (1, "remove_image") => "Logo entfernen",
        (_, "remove_image") => "Remove logo",
        (1, "set_chat_retention") => "Chat-Löschfrist",
        (_, "set_chat_retention") => "Chat retention",
        _ => return None,
    })
}

/// The display title of a proposal payload, in the ACTIVE language: an org
/// governance op renders from its machine placeholder via [`org_op_label`]
/// (even when a legacy payload carries a baked title in some language);
/// everything else falls back to the payload's own user content
/// ([`summarize`]).
pub(crate) fn display_title(lang: i32, v: &serde_json::Value) -> String {
    let op = v.get("op").and_then(serde_json::Value::as_str);
    // a membership record decides about a SEAT (recovery approval design,
    // 2026-08-08) — the member's name is the title's whole point
    if let (Some(op @ ("restore_member" | "add_member")), Some(member)) =
        (op, v.get("member").and_then(serde_json::Value::as_str))
    {
        return match (lang, op) {
            (1, "restore_member") => format!("Sitz wiederherstellen: {member}"),
            (_, "restore_member") => format!("Restore seat: {member}"),
            (1, _) => format!("Sitz hinzufügen: {member}"),
            (_, _) => format!("Add seat: {member}"),
        };
    }
    // a member-profile change is about ONE seat, so the title names it.
    // These cannot go through `org_op_label` (op-only): the member lives
    // in the payload (`member_profiles_plan.md` §5)
    if let (
        Some(op @ ("set_member_image" | "remove_member_image" | "set_member_desc")),
        Some(member),
    ) = (op, v.get("member").and_then(serde_json::Value::as_str))
    {
        return match (lang, op) {
            (1, "set_member_image") => format!("Bild: {member}"),
            (_, "set_member_image") => format!("Picture: {member}"),
            (1, "remove_member_image") => format!("Bild entfernen: {member}"),
            (_, "remove_member_image") => format!("Remove picture: {member}"),
            (1, _) => format!("Beschreibung: {member}"),
            (_, _) => format!("Description: {member}"),
        };
    }
    // a wiki changeset vote: localized label + the language-neutral
    // count summary the proposer's bridge baked in ("+2 -1 →1 ~34")
    if op == Some("wiki_patch") {
        let label = if lang == 1 { "Wiki-Patch" } else { "Wiki patch" };
        let summary = v.get("summary").and_then(serde_json::Value::as_str).unwrap_or("");
        return if summary.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({summary})")
        };
    }
    op.and_then(|o| org_op_label(lang, o))
        .map(str::to_string)
        .unwrap_or_else(|| summarize(v))
}
