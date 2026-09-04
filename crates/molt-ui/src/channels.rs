// SPDX-License-Identifier: GPL-3.0-or-later
//! Chat channels (chat bus, package B4): pure projection helpers over the
//! engine's channel enumeration, plus the UI-side proposal cache that keeps
//! a patch channel titled and stated after its vote left the Proposed-only
//! read. All engine-data driven - the UI never invents channel state.

use std::collections::HashMap;

use molt_core::{ChannelInfo, ChannelRef, Command, ProposalId, ProposalView, Surface};

use crate::surfaces::{display_title, ChannelRowData};

/// The stable string form of a channel across the Rust↔Slint boundary:
/// `"group"`, `"patch:<id>"`, `"topic:<name>"`. Sidebar rows carry it; the
/// `select-channel` callback hands it back.
pub(crate) fn channel_key(c: &ChannelRef) -> String {
    match c {
        ChannelRef::Group => "group".to_string(),
        ChannelRef::Patch { id } => format!("patch:{}", id.0),
        ChannelRef::Topic { name } => format!("topic:{name}"),
    }
}

/// Parse a sidebar channel key back into a [`ChannelRef`]. `None` on junk —
/// a stale or malformed key must never panic the UI.
pub(crate) fn parse_channel_key(key: &str) -> Option<ChannelRef> {
    if key == "group" {
        return Some(ChannelRef::Group);
    }
    if let Some(id) = key.strip_prefix("patch:") {
        return id.parse().ok().map(|id| ChannelRef::Patch { id: ProposalId(id) });
    }
    // TRIMMED here, the same rule `ChannelRef::normalized` applies on send —
    // otherwise the dialog could select "  " as a channel and the failure
    // would only surface on the first message. No stored topic name can carry
    // outer whitespace either, so trimming never misses an existing channel.
    key.strip_prefix("topic:")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| ChannelRef::Topic {
            name: name.to_string(),
        })
}

/// The sidebar's DISCUSSION rows from the engine enumeration: a discussion
/// is vote-bound, so only patch channels whose proposal is still OPEN
/// (something can be voted on — [`KnownFate::Pending`]) appear, by
/// ascending proposal id with the proposal-state title. No group row (the
/// Gruppe view above covers it), no sealed/closed votes, no unknown
/// proposals, no free topics — the engine's channel enumeration itself
/// stays complete (MCP reads it unfiltered).
pub(crate) fn derive_channels(
    lang: i32,
    infos: &[ChannelInfo],
    known: &HashMap<u64, KnownProposal>,
    unread: &HashMap<String, usize>,
) -> Vec<ChannelRowData> {
    let unread_of =
        |key: &str| i32::try_from(unread.get(key).copied().unwrap_or(0)).unwrap_or(i32::MAX);
    // TOPICS first, by name: a human named them, they do not come and go
    // with a vote's lifecycle, and `chat_bus.md` §Phase-4 lists them ("Topic
    // channels as they occur"). They were dropped from this list once and it
    // made the New-topic button a trapdoor - the channel existed, held
    // messages, and had nowhere to be clicked back to.
    let mut topics: Vec<&str> = infos
        .iter()
        .filter_map(|i| match &i.channel {
            ChannelRef::Topic { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    topics.sort_unstable();
    topics.dedup();
    let mut rows: Vec<ChannelRowData> = topics
        .into_iter()
        .map(|name| {
            let key = format!("topic:{name}");
            ChannelRowData {
                unread: unread_of(&key),
                key,
                label: name.to_string(),
                icon: "🏷️".to_string(),
            }
        })
        .collect();
    // …then the discussions of OPEN votes, by id
    let mut patches: Vec<u64> = Vec::new();
    for i in infos {
        if let ChannelRef::Patch { id } = &i.channel {
            patches.push(id.0);
        }
    }
    patches.sort_unstable();
    patches.dedup();
    rows.extend(patches.into_iter().filter_map(|id| {
        let k = known.get(&id)?;
        if k.fate != KnownFate::Pending {
            return None;
        }
        let key = format!("patch:{id}");
        Some(ChannelRowData {
            unread: unread_of(&key),
            key,
            label: display_title(lang, &k.payload),
            icon: "🗳️".to_string(),
        })
    }));
    rows
}

/// The command the patch-channel banner's "back to the vote" button
/// issues: a discussion is vote-bound (the channel key IS the proposal
/// id), so the jump reuses the sidebar's own navigation verbs. The card
/// sits in the hosting surface's view for the proposal's fate, named in
/// that surface's own view vocabulary ([`Surface::views`]): Organization
/// and Shared Files split pending/declined/accepted, Memory
/// proposals/denied/accepted, a surface without split outcomes shows
/// every fate under "proposals". A plain `SelectSurface` would land on
/// the default view (Temporary Uploads, the wiki), where there is no
/// ballot; it stays the fallback for a surface with none of these views.
/// A cache miss falls back to Organization → pending rather than a dead
/// button. Non-patch channels have no vote.
pub(crate) fn vote_jump_command(ch: &ChannelRef, known: &HashMap<u64, KnownProposal>) -> Option<Command> {
    let ChannelRef::Patch { id } = ch else {
        return None;
    };
    let (surface, fate) = known
        .get(&id.0)
        .map(|k| (k.surface, k.fate))
        .unwrap_or((Surface::Organization, KnownFate::Pending));
    let candidates: &[&str] = match fate {
        KnownFate::Pending => &["pending", "proposals"],
        KnownFate::Closed => &["declined", "denied", "proposals"],
        KnownFate::Applied => &["accepted", "proposals"],
    };
    let view = candidates
        .iter()
        .find(|c| surface.views().iter().any(|(k, _)| k == *c));
    Some(match view {
        Some(view) => Command::SelectView { surface, view: (*view).to_string() },
        None => Command::SelectSurface { surface },
    })
}

/// The compose-banner label of the selected channel ("" = group, which
/// needs no banner). For a fresh topic this is the ONLY visible feedback
/// until its first message exists (a channel exists because a message
/// exists), so it must not depend on the sidebar list.
pub(crate) fn channel_display_label(c: &ChannelRef, titles: &HashMap<u64, String>) -> String {
    match c {
        ChannelRef::Group => String::new(),
        ChannelRef::Patch { id } => titles
            .get(&id.0)
            .cloned()
            .unwrap_or_else(|| format!("#{}", id.0)),
        ChannelRef::Topic { name } => name.clone(),
    }
}

/// Whether the selected channel is a DECIDED vote's discussion — read-only
/// for new messages/shares (the engine refuses them with
/// `DiscussionClosed`; this flag collapses the compose row and shows the
/// banner note BEFORE anyone types into a refusal). The engine's channel
/// annotation ([`ChannelInfo::state`]) is authoritative when present; a
/// channel not (yet) in the enumeration — or an unannotated ref — falls
/// back to the UI's proposal cache ([`KnownProposal::fate`]). Group/Topic,
/// open votes and unknown referents are writable (`false`).
pub(crate) fn selected_channel_closed(
    selected: &ChannelRef,
    infos: &[ChannelInfo],
    known: &HashMap<u64, KnownProposal>,
) -> bool {
    let ChannelRef::Patch { id } = selected else {
        return false;
    };
    if let Some(state) = infos
        .iter()
        .find(|i| &i.channel == selected)
        .and_then(|i| i.state)
    {
        return state != molt_core::ProposalState::Proposed;
    }
    known.get(&id.0).is_some_and(|k| k.fate != KnownFate::Pending)
}

/// Whether the selected channel is an **Organization** decision's discussion.
///
/// One flag, two jobs, and they belong together: it puts the compact detail
/// panel above that chat (so opening a decision's discussion always says
/// which decision, without scrolling), and it keeps the nav's Organization
/// section expanded while the chat pane is showing — otherwise the row the
/// user just clicked collapses out of sight the moment it works.
///
/// Deliberately Organization only: the ask says other surfaces' decisions
/// are handled differently, so they get neither.
pub(crate) fn selected_channel_org(selected: &ChannelRef, known: &HashMap<u64, KnownProposal>) -> bool {
    let ChannelRef::Patch { id } = selected else {
        return false;
    };
    known
        .get(&id.0)
        .is_some_and(|k| k.surface == Surface::Organization)
}

/// What the UI remembers about a proposal beyond the read contract's
/// Proposed-only `pending` window. The engine never re-exposes a terminal
/// proposal (a sealed block's `applied` value is the bare payload, without
/// the proposal id), so title and governance state would vanish from the
/// patch channel the moment a block seals — this cache keeps them.
#[derive(Clone)]
pub(crate) struct KnownProposal {
    /// The full payload; the fate probe matches it against the applied log.
    /// Titles are NOT cached: they render from the payload's machine
    /// placeholder in the active language at display time
    /// ([`display_title`]) — a cached rendered string would freeze the
    /// language of the moment it was seen.
    pub(crate) payload: serde_json::Value,
    /// The gated surface the proposal targets (whose applied log to probe).
    pub(crate) surface: Surface,
    /// Approvals at the last sighting in `pending`.
    pub(crate) approvals: usize,
    /// The threshold at the last sighting.
    pub(crate) threshold: usize,
    /// The lifecycle as this UI resolved it (see [`KnownFate`]).
    pub(crate) fate: KnownFate,
}

/// The UI-side proposal lifecycle, resolved from the data the read
/// contract exposes (the contract itself is frozen — no engine change).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KnownFate {
    /// Still in the engine's Proposed-only `pending` read.
    Pending,
    /// Vanished from `pending` and its payload appeared in the surface's
    /// applied log — the block sealed.
    Applied,
    /// Vanished without an applied trace. The read contract cannot
    /// distinguish Rejected from expired/otherwise closed, so the UI
    /// renders a neutral closed marker — never a fabricated verdict.
    Closed,
}

/// Fold one read pass into the proposal cache: every pending proposal is
/// (re-)cached, and every cached proposal that vanished from the
/// Proposed-only window resolves its fate by probing the applied log of
/// its surface. Applied values are the raw proposal payloads (both the
/// chain projection and the legacy simulation push `payload` verbatim, and
/// neither embeds the proposal id), so payload equality is the only match
/// the read contract allows — two byte-identical proposals are therefore
/// indistinguishable here, which at worst upgrades a closed twin to ✓.
/// `Applied` is sticky; `Closed` re-probes, so an out-of-order read that
/// briefly missed the applied value corrects itself on the next pass. A
/// surface missing from `applied` (failed read) resolves nothing.
pub(crate) fn update_known_proposals(
    known: &mut HashMap<u64, KnownProposal>,
    pending: &[ProposalView],
    declined: &[ProposalView],
    applied: &HashMap<Surface, Vec<serde_json::Value>>,
) {
    for p in pending {
        known.insert(
            p.id.0,
            KnownProposal {
                payload: p.payload.clone(),
                surface: p.surface,
                approvals: p.approvals,
                threshold: p.threshold,
                fate: KnownFate::Pending,
            },
        );
    }
    for (id, k) in known.iter_mut() {
        if pending.iter().any(|p| p.id.0 == *id) || k.fate == KnownFate::Applied {
            continue;
        }
        let Some(vals) = applied.get(&k.surface) else {
            continue;
        };
        k.fate = if vals.contains(&k.payload) {
            KnownFate::Applied
        } else {
            KnownFate::Closed
        };
    }
    // the snapshots' declined lists are AUTHORITATIVE Rejected knowledge
    // (the engine names the veto) — fold them last: a veto this UI never
    // saw pending still gets a titled Closed entry, an out-of-order
    // payload-probe verdict is overridden, but an Applied fate is never
    // downgraded (the probe proved the seal; the byte-identical-twin
    // ambiguity must not un-seal it here).
    for p in declined {
        let entry = known.entry(p.id.0).or_insert_with(|| KnownProposal {
            payload: p.payload.clone(),
            surface: p.surface,
            approvals: p.approvals,
            threshold: p.threshold,
            fate: KnownFate::Closed,
        });
        if entry.fate != KnownFate::Applied {
            entry.payload = p.payload.clone();
            entry.fate = KnownFate::Closed;
        }
    }
}

/// The lazy patch-channel titles (sidebar rows + compose banner), from the
/// proposal cache — so a title survives the proposal leaving `pending`.
pub(crate) fn known_titles(lang: i32, known: &HashMap<u64, KnownProposal>) -> HashMap<u64, String> {
    known
        .iter()
        .map(|(id, k)| (*id, display_title(lang, &k.payload)))
        .collect()
}
