// SPDX-License-Identifier: GPL-3.0-or-later
//! The chat log's projection: one typed message to its display row, the
//! whole-log pass (author blocks, quote teasers, reply indent), the
//! UI-synthesized governance lines of a patch channel and their merge into
//! the engine-ordered log.

use std::collections::HashMap;

use molt_core::{ChannelRef, ChatMessage, ProposalView, SurfaceSnapshot};

use crate::labels::{file_date_label, file_size_label, when_label};
use crate::channels::{KnownFate, KnownProposal};
use crate::surfaces::{display_title, LogLineData, ReactionData, ReceiptData};

/// One quotable message, as the teaser renderer needs it — built over the
/// FULL chat log, because a quote may point across channels (the sanctioned
/// cross-post) and must still tease when its target is filtered out of view.
pub(crate) struct QuoteSrc {
    pub(crate) lead: String,
    pub(crate) text: String,
    pub(crate) deleted: bool,
}

/// Everything the chat surface's projection needs beyond its own (possibly
/// channel-filtered) snapshot: the selected channel, the collected proposal
/// state feeding the patch-channel system lines (P8), the UI-side
/// first-seen times standing in for the timestamps proposals do not carry,
/// and the FULL log's quote sources (see [`QuoteSrc`]).
pub(crate) struct ChatViewCtx {
    pub(crate) selected: ChannelRef,
    pub(crate) proposals: Vec<ProposalView>,
    /// The per-workspace proposal cache (title + fate survive a proposal
    /// leaving the Proposed-only `pending` window).
    pub(crate) known: HashMap<u64, KnownProposal>,
    pub(crate) first_seen: HashMap<u64, u64>,
    pub(crate) quotes: HashMap<String, QuoteSrc>,
    /// The full member roster (names) — the universe of read-receipt dots
    /// per message (every member except the author).
    pub(crate) roster: Vec<String>,
}

/// The typed chat messages of a snapshot (chat surface only).
pub(crate) fn chat_messages(snap: &SurfaceSnapshot) -> Vec<ChatMessage> {
    snap.applied
        .iter()
        .filter_map(|v| serde_json::from_value::<ChatMessage>(v.clone()).ok())
        .collect()
}

/// One typed chat message, projected for display. Quote resolution (row +
/// teaser) happens later in [`annotate_chat_log`]: the row index can only
/// be known once system lines are merged in, and the teaser may resolve
/// against a message outside the displayed (filtered) log.
pub(crate) fn chat_line(lang: i32, m: &ChatMessage, me: &str, roster: &[String]) -> LogLineData {
    let mut mine_emoji = String::new();
    // read receipts are for the SENDER: they appear only on YOUR OWN messages,
    // so you see who has read what you sent — never a row on incoming messages
    // (as the receiver you don't need to know you read it). One dot per OTHER
    // member, green once they have read it (in `read_by`), yellow until then.
    // Only real, live, human own-messages carry them; the .slint additionally
    // hides the row when the local read-receipts switch is off (symmetric).
    let receipts: Vec<ReceiptData> = if m.from != me
        || m.id.is_nil()
        || !m.kind.is_user()
        || m.deleted_by.is_some()
    {
        Vec::new()
    } else {
        roster
            .iter()
            .filter(|name| name.as_str() != m.from)
            .map(|name| ReceiptData {
                name: name.clone(),
                read: m.read_by.contains(name),
            })
            .collect()
    };
    // the BTreeMap iterates sorted by emoji, so the pill order is
    // deterministic across re-renders
    let reactions: Vec<ReactionData> = m
        .reactions
        .iter()
        .map(|(emoji, who)| {
            let mine = who.iter().any(|w| w == me);
            if mine {
                mine_emoji = emoji.clone();
            }
            ReactionData {
                emoji: emoji.clone(),
                count: i32::try_from(who.len()).unwrap_or(i32::MAX),
                mine,
            }
        })
        .collect();
    // a shared file renders as a card: name plus "size · type · date"
    let (has_file, file_name, file_meta, file_available) = match &m.file {
        Some(f) => (
            true,
            f.name.clone(),
            format!(
                "{} · {} · {}",
                file_size_label(f.size),
                f.kind,
                file_date_label(f.modified)
            ),
            f.available,
        ),
        None => (false, String::new(), String::new(), false),
    };
    LogLineData {
        id: if m.id.is_nil() {
            String::new() // a legacy entry: not addressable until B1
        } else {
            m.id.to_string()
        },
        lead: m.from.clone(),
        // system lines drop an embedded raw diff (legacy decision
        // summaries carried one; the card's raw-patch button shows it)
        text: if m.kind.is_user() {
            m.body.clone()
        } else {
            strip_diff_body(&m.body)
        },
        when: if m.ts > 0 {
            when_label(lang, m.ts)
        } else {
            String::new()
        },
        // legacy numeric quote only (pre-chat-bus rows; B1 resolves these
        // to quote_id at ingest, after which this path goes dormant) — the
        // id path leaves it to annotate_chat_log
        quote: if m.quote_id.is_none() {
            m.quote
                .and_then(|q| i32::try_from(q).ok())
                .unwrap_or(-1)
        } else {
            -1
        },
        quote_id: m.quote_id.map(|q| q.to_string()).unwrap_or_default(),
        // an engine-authored notice (ChatKind::System, e.g. the recovery
        // rejoin announcement) rides the same quiet-line rendering as the
        // UI-synthesized governance rows — one flag, no second style
        system: !m.kind.is_user(),
        quote_label: String::new(), // teaser, filled in by annotate_chat_log
        quote_indent: 0,            // reply depth, filled in by annotate_chat_log
        deleted_by: m.deleted_by.clone().unwrap_or_default(),
        first: true, // author-block start, filled in by annotate_chat_log
        own: m.from == me,
        alt: false, // author-block zebra, filled in by annotate_chat_log
        mine_emoji,
        reactions,
        receipts,
        has_file,
        proposal_id: None,
        file_name,
        file_meta,
        file_available,
    }
}

/// The whole-log pass over a chat: author-block zebra (the stripe flips
/// whenever a DIFFERENT author takes over), the once-per-block header flag,
/// and the quote teasers. Id-based quotes resolve their teaser through
/// `quotes` (built over the FULL log — a cross-channel quote teases even
/// when its target is filtered out of view) and their jump row by scanning
/// the displayed log; legacy numeric quotes resolve by row as before.
pub(crate) fn annotate_chat_log(log: &mut [LogLineData], quotes: &HashMap<String, QuoteSrc>) {
    let mut alt = false;
    let mut prev_lead: Option<String> = None;
    for line in log.iter_mut() {
        if line.system {
            // a governance line is transparent to the author-block rhythm:
            // the surrounding block keeps its stripe and shows no header
            line.first = false;
            line.alt = alt;
            continue;
        }
        if prev_lead.as_deref().is_some_and(|p| p != line.lead) {
            alt = !alt;
        }
        line.alt = alt;
        // the author header (name + time) shows once per author block
        line.first = prev_lead.as_deref() != Some(line.lead.as_str());
        prev_lead = Some(line.lead.clone());
    }
    // id → displayed row: the jump target of an in-view quote
    let row_of: HashMap<String, usize> = log
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.id.is_empty())
        .map(|(i, l)| (l.id.clone(), i))
        .collect();
    for i in 0..log.len() {
        if !log[i].quote_id.is_empty() {
            let qid = log[i].quote_id.clone();
            log[i].quote = row_of
                .get(&qid)
                .and_then(|r| i32::try_from(*r).ok())
                .unwrap_or(-1);
            match quotes.get(&qid) {
                Some(src) if !src.deleted => {
                    log[i].quote_label = format!("{}: {}", src.lead, src.text);
                }
                Some(src) => log[i].quote_label = format!("{}: …", src.lead),
                // not in the full-log map either: dangling — drop the quote
                None => log[i].quote = -1,
            }
        } else if log[i].quote >= 0 {
            // legacy numeric quote (pre-chat-bus rows; B1 resolves these to
            // quote_id at ingest, after which this path goes dormant)
            let q = log[i].quote;
            match usize::try_from(q).ok().and_then(|q| log.get(q)) {
                Some(src) if src.deleted_by.is_empty() => {
                    log[i].quote_label = format!("{}: {}", src.lead, src.text);
                }
                Some(src) => log[i].quote_label = format!("{}: …", src.lead),
                None => log[i].quote = -1,
            }
        }
    }
    // the reply indent: consecutive quote rows of the SAME target share one
    // depth, a neighbor quoting a DIFFERENT target takes the other — so
    // stacked replies to different questions stop reading as one thread. A
    // non-quoting row ends the run (the next group starts at depth 1).
    // Runs AFTER the teaser pass: only rows whose quote actually renders
    // (quote_label set) may indent.
    let mut depth = 0;
    for i in 0..log.len() {
        if log[i].quote_label.is_empty() {
            depth = 0;
            continue;
        }
        if i == 0 || !same_quote_target(&log[i - 1], &log[i]) {
            depth = if depth == 1 { 2 } else { 1 };
        }
        log[i].quote_indent = depth;
    }
}

/// Whether two displayed rows quote the SAME target — the grouping relation
/// behind the alternating reply indent. Precedence: the resolved target row
/// (set for both id and legacy quotes whose target is in view — so the two
/// addressing styles agree on a shared target), then the stable id, then the
/// teaser text as the last resort for unresolvable cross-channel quotes.
fn same_quote_target(a: &LogLineData, b: &LogLineData) -> bool {
    if a.quote_label.is_empty() || b.quote_label.is_empty() {
        return false;
    }
    if a.quote >= 0 && b.quote >= 0 {
        return a.quote == b.quote;
    }
    if !a.quote_id.is_empty() || !b.quote_id.is_empty() {
        return a.quote_id == b.quote_id;
    }
    a.quote_label == b.quote_label
}

/// Quote-teaser sources over the FULL chat log, keyed by hex message id.
pub(crate) fn quote_sources(msgs: &[ChatMessage]) -> HashMap<String, QuoteSrc> {
    msgs.iter()
        .filter(|m| !m.id.is_nil())
        .map(|m| {
            (
                m.id.to_string(),
                QuoteSrc {
                    lead: m.from.clone(),
                    text: m.body.clone(),
                    deleted: m.deleted_by.is_some(),
                },
            )
        })
        .collect()
}

/// A UI-synthesized governance line (P8): no author, no id — so the
/// id-requiring row actions stay hidden by the same guard that protects
/// legacy rows — rendered quiet via the `system` flag. The text is
/// deliberately symbols + numbers + user content ("⚖ #4 · title — 2/3"),
/// so it reads the same in every language and needs no lexicon entry.
/// Legacy decision summaries embedded the raw diff (the cap kept 160
/// chars of "diff --git …"); the chat renders the head only — the card's
/// raw-patch button is the place for the patch.
fn strip_diff_body(text: &str) -> String {
    match text.find("diff --git") {
        Some(pos) => text[..pos].trim_end().to_string(),
        None => text.to_string(),
    }
}

pub(crate) fn system_line_data(text: String) -> LogLineData {
    LogLineData {
        id: String::new(),
        lead: String::new(),
        text,
        when: String::new(),
        quote: -1,
        quote_id: String::new(),
        system: true,
        quote_label: String::new(),
        quote_indent: 0,
        deleted_by: String::new(),
        first: false,
        own: false,
        alt: false,
        mine_emoji: String::new(),
        reactions: Vec::new(),
        receipts: Vec::new(),
        has_file: false,
        file_name: String::new(),
        file_meta: String::new(),
        file_available: false,
        proposal_id: None,
    }
}

/// The system lines of one `Patch(id)` channel, synthesized from proposal
/// state (P8 — a UI-side merge, no engine/wire change). Proposals carry no
/// timestamp, so lines are stamped with the UI's FIRST-SEEN time — an
/// approximation that keeps a line stable within a session and near the
/// governance moment it reports (0 = never seen: sorts to the top). A
/// proposal no longer in the Proposed-only `pending` window renders from
/// the [`KnownProposal`] cache: sealed shows `m/m ✓` (the engine seals a
/// block at exactly the threshold), vanished-without-apply shows the
/// neutral `⊘` (Rejected vs expired is not distinguishable from the read
/// contract). An id known nowhere yields a bare `⚖ #id` line and never an
/// error (concept Q4).
pub(crate) fn patch_system_lines(
    lang: i32,
    patch: u64,
    pending: &[ProposalView],
    known: &HashMap<u64, KnownProposal>,
    first_seen: &HashMap<u64, u64>,
) -> Vec<(u64, LogLineData)> {
    let text = match pending.iter().find(|p| p.id.0 == patch) {
        Some(p) => format!(
            "⚖ #{patch} · {} - {}/{}",
            display_title(lang, &p.payload),
            p.approvals,
            p.threshold
        ),
        None => match known.get(&patch) {
            Some(k) => {
                let progress = match k.fate {
                    KnownFate::Applied => format!("{}/{} ✓", k.threshold, k.threshold),
                    KnownFate::Closed => "⊘".to_string(),
                    KnownFate::Pending => format!("{}/{}", k.approvals, k.threshold),
                };
                format!("⚖ #{patch} · {} - {progress}", display_title(lang, &k.payload))
            }
            None => format!("⚖ #{patch}"),
        },
    };
    let ts = first_seen.get(&patch).copied().unwrap_or(0);
    vec![(ts, system_line_data(text))]
}

/// Merge the system lines into the chat lines by timestamp. The chat log's
/// own order is authoritative (it is the engine's log order) and is never
/// disturbed; a system line ties BEFORE the chat line of the same second.
pub(crate) fn merge_by_time(
    chat: Vec<(u64, LogLineData)>,
    mut system: Vec<(u64, LogLineData)>,
) -> Vec<LogLineData> {
    system.sort_by_key(|(ts, _)| *ts); // stable: equal stamps keep their order
    let mut out = Vec::with_capacity(chat.len() + system.len());
    let mut sys = system.into_iter().peekable();
    for (ts, line) in chat {
        while sys.peek().is_some_and(|(sts, _)| *sts <= ts) {
            out.push(sys.next().expect("peeked").1);
        }
        out.push(line);
    }
    out.extend(sys.map(|(_, l)| l));
    out
}
