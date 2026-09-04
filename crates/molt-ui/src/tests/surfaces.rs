// SPDX-License-Identifier: GPL-3.0-or-later
//! The surfaces bundle: proposal cards, tables, the push epoch, paging.

use super::*;

/// The set_relays vote card shows the CHANGES: every pool member of the
/// union, marked kept / added / removed, in current-then-added order.
/// Review 2026-08-12: a set_features card must never paint a red
/// "removed" row - the union fold cannot remove, and `current` is
/// recomputed live, so a racing enable would otherwise show an
/// impossible removal on a governance card. Keys render as display
/// labels (one vocabulary with nav and wizard).
#[test]
fn a_feature_diff_never_shows_a_removal_and_renders_labels() {
    let pv = ProposalView {
        id: ProposalId(7),
        surface: Surface::Organization,
        payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
        approvals: 1,
        threshold: 2,
        state: ProposalState::Proposed,
        approved_by_me: false,
        declined_by_me: false,
        // a racing enable made "vault" effective AFTER this was proposed
        current: "memory vault".to_string(),
        proposed: "memory quests".to_string(),
        votes: Vec::new(),
        declined_at: 0,
        declined_by: String::new(),
        by: String::new(),
        mine: false,
        superseded: false,
        withdrawn: false,
    };
    let row = proposal_row(0, &pv);
    assert!(
        row.relay_changes.iter().all(|(sign, _)| *sign != RELAY_ROW_REMOVED),
        "a feature diff row claimed a removal: {:?}",
        row.relay_changes
    );
    assert!(
        row.relay_changes
            .iter()
            .any(|(sign, label)| *sign == RELAY_ROW_KEPT && label == "Vault"),
        "the racing enable renders as kept, labelled: {:?}",
        row.relay_changes
    );
    assert!(
        row.relay_changes
            .iter()
            .any(|(sign, label)| *sign == RELAY_ROW_ADDED && label == "Kanban"),
        "the addition renders with its display label: {:?}",
        row.relay_changes
    );
}

#[test]
fn relay_pool_diff_marks_added_removed_kept() {
    let rows = relay_pool_diff("wss://a wss://b", "wss://b wss://c");
    assert_eq!(
        rows,
        vec![
            (RELAY_ROW_REMOVED, "wss://a".to_string()),
            (RELAY_ROW_KEPT, "wss://b".to_string()),
            (RELAY_ROW_ADDED, "wss://c".to_string()),
        ]
    );
    // identical pools: everything kept, nothing invented
    assert_eq!(
        relay_pool_diff("wss://a", "wss://a"),
        vec![(RELAY_ROW_KEPT, "wss://a".to_string())]
    );
    // duplicates in a hand-written proposal collapse
    assert_eq!(
        relay_pool_diff("", "wss://x wss://x"),
        vec![(RELAY_ROW_ADDED, "wss://x".to_string())]
    );
    // an empty proposed pool folds as a no-op engine-side, so the card
    // must NOT promise removals — no rows, generic fallback
    assert_eq!(relay_pool_diff("wss://a", ""), Vec::<(i32, String)>::new());
}

/// A profile change is about ONE seat - the card says whose.
#[test]
fn member_profile_titles_name_the_seat_in_both_languages() {
    for (op, en, de) in [
        ("set_member_image", "Picture: walter", "Bild: walter"),
        (
            "set_member_desc",
            "Description: walter",
            "Beschreibung: walter",
        ),
        (
            "remove_member_image",
            "Remove picture: walter",
            "Bild entfernen: walter",
        ),
    ] {
        let payload = serde_json::json!({ "op": op, "member": "walter" });
        assert_eq!(display_title(0, &payload), en);
        assert_eq!(display_title(1, &payload), de);
    }
    // a profile payload without a seat cannot claim one
    let anon = serde_json::json!({ "op": "set_member_desc", "value": "hi" });
    assert!(!display_title(0, &anon).contains("Description:"));
}

/// Discussion/card titles must never mix languages: an org governance
/// payload carries the machine `op` as its placeholder and the UI
/// translates it AT RENDER TIME in the active language — never a
/// pre-rendered string frozen in whatever language the proposer's UI
/// happened to be in. User content (note titles) passes through.
#[test]
fn org_titles_render_in_the_active_language_from_the_op_placeholder() {
    let payload = serde_json::json!({"op": "set_name", "value": "Neu"});
    assert_eq!(display_title(0, &payload), "Rename");
    assert_eq!(display_title(1, &payload), "Name ändern");
    // a legacy payload with a baked, possibly foreign-language title:
    // the op placeholder still wins for governance ops
    let legacy =
        serde_json::json!({"op": "set_image", "title": "Logo ändern", "value": "x.png"});
    // short noun labels: the sidebar channel list elides long titles,
    // and a leading "Change …" verb is redundant on a proposal anyway
    assert_eq!(display_title(0, &legacy), "Logo");
    // user content is the title — untouched, in any language
    let note = serde_json::json!({"op": "add_note", "title": "budget"});
    assert_eq!(display_title(0, &note), "budget");
    assert_eq!(display_title(1, &note), "budget");
    // the two ops that carry no user title at all must not leak their
    // op code (user report 2026-08-28: "set_features" on the card)
    let features = serde_json::json!({"op": "set_features", "value": "memory vault"});
    assert_eq!(display_title(0, &features), "Features");
    assert_eq!(display_title(1, &features), "Features");
    let relays = serde_json::json!({"op": "set_relays", "value": "wss://a"});
    assert_eq!(display_title(0, &relays), "Relay pool");
    assert_eq!(display_title(1, &relays), "Relay-Pool");
}

/// WP1: an applied log line carries the id of the proposal that produced
/// it (the snapshot's parallel id track), so the row can offer the 💬
/// jump into the vote's discussion. A row with no known origin (legacy
/// dump, pre-id peer) carries none and must offer no jump.
#[test]
fn applied_log_lines_carry_their_patch_id() {
    let snap = molt_core::SurfaceSnapshot {
        surface: Surface::Memory,
        gated: true,
        applied: vec![
            serde_json::json!({"op": "add_note", "title": "a"}),
            serde_json::json!({"op": "add_note", "title": "b"}),
        ],
        applied_ids: vec![Some(7), None],
        pending: Vec::new(),
        denied: 0,
        declined: Vec::new(),
        accepted: vec![ProposalView {
            id: ProposalId(7),
            surface: Surface::Memory,
            payload: serde_json::json!({"op": "add_note", "title": "a"}),
            approvals: 2,
            threshold: 2,
            state: molt_core::ProposalState::Applied,
            approved_by_me: true,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: vec![
                molt_core::MemberVote {
                    member: "petra".to_string(),
                    vote: molt_core::VoteState::Approved,
                },
                molt_core::MemberVote {
                    member: "walter".to_string(),
                    vote: molt_core::VoteState::Approved,
                },
            ],
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        }],
        channels: Vec::new(),
        has_archive: false,
        wiki_docs: 0,
        wiki_rev: 0,
    };
    let data = surface_data(0, Surface::Memory, &snap, "petra", None, &HashMap::new());
    assert_eq!(data.log.len(), 2);
    assert_eq!(data.log[0].proposal_id, Some(7));
    assert_eq!(data.log[1].proposal_id, None);
    // the Accepted table: newest first, the proposal-backed row carries
    // its voters, the legacy row (unknown origin) only its title
    assert_eq!(data.accepted.len(), 2);
    assert_eq!(data.accepted[0].id, -1, "legacy row, no discussion jump");
    assert_eq!(data.accepted[1].id, 7);
    assert_eq!(data.accepted[1].votes.len(), 2, "the block-proven voters");
}

/// The epoch invalidates a bundle read for a selection the user has
/// LEFT — that is the whole job. It used to invalidate on every newer
/// push start as well, which starved the pane (see
/// `an_overlapping_push_does_not_starve_the_one_it_overlaps`): a stale
/// bundle landing is a cosmetic revert one push later, an empty pane is
/// the user losing their chat.
#[test]
fn push_generation_guard_invalidates_stale_pushes() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-1");
    let g1 = st.begin_push("ws-1").expect("current");
    assert!(st.is_current(g1), "a push for the current selection lands");
    // a selection change invalidates every in-flight push …
    st.select(ChannelRef::Topic {
        name: "budget".into(),
    });
    assert!(!st.is_current(g1));
    assert_eq!(
        st.selected,
        ChannelRef::Topic {
            name: "budget".into()
        }
    );
    // … and the counter moves across the workspace-switch reset, so an
    // old push can never match a freshly reset state
    let g2 = st.begin_push("ws-1").expect("current");
    st.enter_workspace("ws-2");
    let g3 = st.begin_push("ws-2").expect("current");
    assert!(g3 > g2, "monotonic across enter_workspace resets");
    assert!(st.is_current(g3));
    assert!(!st.is_current(g2));
}

/// A workspace switch must not leak the previous workspace's channel
/// state into the next one: a stale Patch/Topic selection would filter
/// the new workspace's log until manually cleared, and the first-seen
/// stamps would misplace system lines. Same workspace → everything is
/// kept. (Unread counts live engine-side since B2 and reset with the
/// workspace there.)
#[test]
fn chat_ui_state_resets_on_workspace_switch() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-1");
    st.selected = ChannelRef::Topic {
        name: "budget".to_string(),
    };
    st.first_seen.insert(4, 100);

    // the same workspace: selection and stamps survive
    st.enter_workspace("ws-1");
    assert_eq!(
        st.selected,
        ChannelRef::Topic {
            name: "budget".to_string()
        }
    );
    assert_eq!(st.first_seen.get(&4), Some(&100));

    // a switch: back to Group, stamps gone, and the new identity sticks
    st.enter_workspace("ws-2");
    assert_eq!(st.selected, ChannelRef::Group);
    assert!(st.first_seen.is_empty());
    st.selected = ChannelRef::Group;
    st.enter_workspace("ws-2");
    assert!(st.first_seen.is_empty(), "no reset without a switch");
}

// ---- the chat pane's push epoch -----------------------------------

/// **Two overlapping pushes must BOTH be able to land.**
///
/// `push_surfaces` issues `MarkChannelRead` whenever the channel on
/// screen has unread messages; the engine event that causes starts the
/// next push while the current one is still reading. While `begin_push`
/// bumped the epoch, that made the reading push stale and it threw its
/// finished bundle away — so opening a chat with anything unread left
/// the pane EMPTY until some later burst happened to leave one push
/// unoverlapped. That is the bug this pins, and it is invisible to any
/// test that pushes one at a time.
#[test]
fn an_overlapping_push_does_not_starve_the_one_it_overlaps() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-1");
    let a = st.begin_push("ws-1").expect("the active workspace");
    let b = st.begin_push("ws-1").expect("the MarkChannelRead echo");
    assert!(st.is_current(b), "the newer push lands");
    assert!(
        st.is_current(a),
        "…and so does the one it overlapped: both read the same selection, \
         so dropping either renders nothing at all"
    );
}

/// **THE first-open bug, from the user's own log.**
///
/// ```text
/// ui: workspace switch from= to=752… gen=2
/// ui: bundle gathered ws=752… gen=2 channel=group chat_rows=9
/// ui: bundle DROPPED as stale gen=2
/// ```
///
/// The bundle was RIGHT — nine rows — and was thrown away 38 ms later
/// because the epoch had moved. What moved it was the session mirror
/// refreshing the CREATE WIZARD's relay picker: opening a workspace
/// changes the dialable pool, `set_create_relays` bumped, and the
/// surfaces bundle in flight died of it. Only on the first open,
/// because the pool only changes once — which is exactly the reported
/// symptom.
///
/// The epoch is the SELECTION epoch. It exists so a bundle read for a
/// channel or workspace the user has left cannot land. A relay picker
/// the bundle does not even carry must not be able to invalidate it.
#[test]
fn unrelated_ui_state_cannot_stale_a_surfaces_bundle() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-1");
    let in_flight = st.begin_push("ws-1").expect("current");

    // the session mirror refreshes the create wizard's relay picker —
    // which the surfaces bundle does not carry at all
    st.set_create_relays(vec!["wss://relay.example".to_string()]);
    assert!(
        st.is_current(in_flight),
        "the relay picker is not part of the bundle - it must not stale it"
    );

    // …and the things the bundle DOES carry still do
    let in_flight = st.begin_push("ws-1").expect("current");
    st.sort_members_by("name");
    assert!(
        !st.is_current(in_flight),
        "the members order IS in the bundle - a stale one would revert it"
    );
}

/// **A push reading for a workspace that is no longer open must not
/// land, and must not drag the state back to it.**
///
/// This is the empty chat on a first open. The workspace switch used to
/// ride `begin_push`, keyed on whatever session copy that push had read
/// — so a push that read the session BEFORE the open re-entered the
/// state as "no workspace" AFTER it, bumped the epoch past the good
/// push (whose bundle was then discarded) and landed its own empty one.
/// Switching surfaces forced a fresh push, which is why it looked like
/// the chat needed a nudge.
#[test]
fn a_push_that_read_the_session_before_an_open_cannot_land_after_it() {
    let mut st = ChatUiState::default();
    // …a push that read the session while nothing was open
    let stale = st.begin_push("").expect("nothing open is a state too");
    // …then the open lands, through the SESSION mirror
    st.enter_workspace("ws-1");
    let fresh = st.begin_push("ws-1").expect("the open workspace");

    assert!(st.is_current(fresh), "the push that read the open workspace lands");
    assert!(!st.is_current(stale), "…and the one from before it does not");
    // the decisive part: the stale push cannot re-enter the old state
    assert_eq!(
        st.begin_push(""),
        None,
        "a push for a workspace that is not open renders nothing at all"
    );
    assert_eq!(st.workspace, "ws-1", "…and it did not drag the state back");
}

/// The epoch exists for ONE thing: a bundle read for a selection the
/// user has left must never land on the one they are looking at (it
/// would also mark the wrong channel read).
#[test]
fn a_push_read_for_another_selection_never_lands() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-1");
    let in_flight = st.begin_push("ws-1").expect("current");
    st.select(ChannelRef::Topic { name: "budget".into() });
    assert!(
        !st.is_current(in_flight),
        "a bundle read for the previous channel must not land"
    );
    // …and a workspace switch is the same rule one level up
    let in_flight = st.begin_push("ws-1").expect("current");
    st.enter_workspace("ws-2");
    assert!(
        !st.is_current(in_flight),
        "a bundle read against another workspace's log must not land"
    );
}

/// The chat offers exactly ONE view, and it is writable. The nav used
/// to carry two more: an Archive (the older half of the retention
/// window - an invisible cliff a conversation fell over at 3.5 days)
/// and the agent-facing "unread" slice, which broke the pane outright:
/// the GUI marks the on-screen channel read on every refresh, so it
/// emptied itself on sight, and the compose row is gated on the general
/// view, so there was nothing to write into either.
#[test]
fn the_chat_offers_one_writable_view() {
    assert_eq!(
        Surface::Chat.views().iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        ["today"],
        "a second chat view is a place a user can get stranded in"
    );
    assert_eq!(Surface::Chat.default_view(), "today");
    // …and the read slice stays available to an agent, off the nav
    assert!(molt_core::CHAT_READ_SLICES.contains(&"unread"));
}

/// Uploads-table row for the presentation tests. The DISPLAY strings
/// are deliberately misleading (they would sort the other way round),
/// pinning that date/size/expiry sort by the underlying numeric keys
/// and never the rendered labels.
fn upload(user: &str, name: &str, checksum: &str, ts: u64, bytes: u64) -> UploadRowData {
    UploadRowData {
        persistent: false,
        vote: String::new(),
        mirrors: 1,
        mirror_held: 0,
        mirror_of: 0,
        id: String::new(),
        user: user.to_string(),
        date: format!("{}", u64::MAX - ts),
        name: name.to_string(),
        kind: String::new(),
        size: format!("{} KiB", u64::MAX - bytes),
        available: true,
        online: true,
        // the cell shows a shortened prefix — the filter must still
        // match on the full value
        checksum: checksum.get(..4).unwrap_or(checksum).to_string(),
        expires: String::new(),
        status: String::new(),
        status_kind: 0,
        availability: String::new(),
        ts,
        bytes,
        expires_ts: ts,
        checksum_full: checksum.to_string(),
    }
}

#[test]
fn sort_uploads_text_columns_case_insensitive() {
    let mut rows = vec![
        upload("bob", "zeta.pdf", "CC99", 1, 1),
        upload("Alice", "Alpha.PDF", "0b11", 2, 2),
        upload("carol", "beta.txt", "aa22", 3, 3),
    ];
    rows[0].kind = "PDF".to_string();
    rows[1].kind = "zip".to_string();
    rows[2].kind = "Txt".to_string();
    rows[0].status = "\u{2713}".to_string();
    rows[1].status = "42 %".to_string();
    let users = |rows: &[UploadRowData]| -> Vec<String> {
        rows.iter().map(|r| r.user.clone()).collect()
    };
    sort_uploads(&mut rows, "user", true);
    assert_eq!(users(&rows), ["Alice", "bob", "carol"], "case-insensitive");
    sort_uploads(&mut rows, "user", false);
    assert_eq!(users(&rows), ["carol", "bob", "Alice"], "descending flips");
    sort_uploads(&mut rows, "file", true);
    assert_eq!(users(&rows), ["Alice", "carol", "bob"], "Alpha < beta < zeta");
    sort_uploads(&mut rows, "type", true);
    assert_eq!(users(&rows), ["bob", "carol", "Alice"], "pdf < txt < zip");
    sort_uploads(&mut rows, "checksum", true);
    assert_eq!(users(&rows), ["Alice", "carol", "bob"], "0b < aa < cc");
    sort_uploads(&mut rows, "download", true);
    assert_eq!(users(&rows), ["carol", "Alice", "bob"], "idle < 42 % < ✓");
}

#[test]
fn sort_uploads_numeric_columns_use_underlying_values() {
    // the rendered date/size labels would sort exactly the other way
    // round (see `upload`) — only the numeric keys give this order
    let mut rows = vec![
        upload("a", "x", "", 30, 10_240),
        upload("b", "y", "", 10, 2_048),
        upload("c", "z", "", 20, 900),
    ];
    let users = |rows: &[UploadRowData]| -> Vec<String> {
        rows.iter().map(|r| r.user.clone()).collect()
    };
    sort_uploads(&mut rows, "date", true);
    assert_eq!(users(&rows), ["b", "c", "a"], "oldest share first");
    sort_uploads(&mut rows, "date", false);
    assert_eq!(users(&rows), ["a", "c", "b"], "newest share first");
    sort_uploads(&mut rows, "size", true);
    assert_eq!(users(&rows), ["c", "b", "a"], "900 B < 2 KiB < 10 KiB");
    sort_uploads(&mut rows, "expires", true);
    assert_eq!(users(&rows), ["b", "c", "a"], "soonest expiry first");
    // an unknown/empty column keeps the current order
    sort_uploads(&mut rows, "", false);
    assert_eq!(users(&rows), ["b", "c", "a"]);
}

#[test]
fn filter_uploads_matches_user_name_or_checksum_case_insensitively() {
    let all = || {
        vec![
            upload("Alice", "report.pdf", "aabb1122", 1, 1),
            upload("bob", "photo.png", "ccdd3344", 2, 2),
        ]
    };
    assert_eq!(filter_uploads(all(), "").len(), 2, "empty needle = all");
    let f = filter_uploads(all(), "LICE");
    assert_eq!(f.len(), 1, "user match, case-insensitive");
    assert_eq!(f[0].user, "Alice");
    let f = filter_uploads(all(), "PHOTO");
    assert_eq!(f.len(), 1, "filename match");
    assert_eq!(f[0].user, "bob");
    // beyond the 4-char display prefix — must match the FULL checksum
    let f = filter_uploads(all(), "DD33");
    assert_eq!(f.len(), 1, "full-checksum match");
    assert_eq!(f[0].user, "bob");
    assert!(filter_uploads(all(), "zzz").is_empty(), "no match = empty");
}

/// Members-table row for the sort tests.
fn member(name: &str, id: &str, last_ts: u64, state: i32, uploads: i32) -> MemberRowData {
    MemberRowData {
        name: name.to_string(),
        id: id.to_string(),
        pk: id.to_string(),
        last: String::new(),
        last_ts,
        state,
        uploads,
        split: String::new(),
        image: String::new(),
        image_key: String::new(),
        desc: String::new(),
    }
}

#[test]
fn sort_members_by_name_uploads_and_presence() {
    let mut rows = vec![
        member("bob", "0b", 10_000, 0, 3),
        member("Alice", "aa", 9_700, 1, 10),
        member("carol", "", 0, 2, 2),
    ];
    let names = |rows: &[MemberRowData]| -> Vec<String> {
        rows.iter().map(|r| r.name.clone()).collect()
    };
    sort_members(&mut rows, "name", true);
    assert_eq!(names(&rows), ["Alice", "bob", "carol"], "case-insensitive");
    sort_members(&mut rows, "uploads", true);
    assert_eq!(names(&rows), ["carol", "bob", "Alice"], "2 < 3 < 10 numeric");
    sort_members(&mut rows, "uploads", false);
    assert_eq!(names(&rows), ["Alice", "bob", "carol"]);
    // "last" is the REAL stamp: most recent first, never-seen (0) at
    // the end — regardless of pill state
    sort_members(&mut rows, "last", true);
    assert_eq!(names(&rows), ["bob", "Alice", "carol"]);
    // unanchored (empty) identity cells sort last ascending
    sort_members(&mut rows, "id", true);
    assert_eq!(names(&rows), ["bob", "Alice", "carol"], "0b < aa < empty");
    sort_members(&mut rows, "", true);
    assert_eq!(names(&rows), ["bob", "Alice", "carol"], "unknown = keep");
}

/// The Members/Uploads tables' view state: clicking the active column
/// flips the direction, a new column starts ascending, and every
/// change bumps the push generation (stales in-flight bundles).
#[test]
fn org_sort_state_toggles_and_bumps_generation() {
    let mut st = ChatUiState::default();
    let g = st.generation;
    st.sort_uploads_by("size");
    assert_eq!(st.uploads_sort, "size");
    assert!(st.uploads_asc, "a fresh column starts ascending");
    st.sort_uploads_by("size");
    assert!(!st.uploads_asc, "the same column flips the direction");
    st.sort_uploads_by("user");
    assert_eq!(st.uploads_sort, "user");
    assert!(st.uploads_asc, "switching columns resets to ascending");
    st.sort_members_by("uploads");
    assert_eq!(st.members_sort, "uploads");
    assert!(st.members_asc);
    st.set_uploads_filter("alice".to_string());
    assert_eq!(st.uploads_filter, "alice");
    assert_eq!(st.generation, g + 5, "every change stales in-flight pushes");
}

/// The pure paging window behind the proposal-outcome lists
/// (Declined / the applied log): 20 rows per page, the page clamps
/// into range (a shrunk list must never show an empty page), and a
/// list of at most one page reports `page_count == 1` — the pager
/// row hides on that.
#[test]
fn page_slice_windows_and_clamps() {
    // empty list: one (empty) page, never a panic range
    assert_eq!(page_slice(0, 0, 20), (0, 0, 0, 1));
    // exactly one page: untouched
    assert_eq!(page_slice(20, 0, 20), (0, 20, 0, 1));
    // one entry over: a second page holding the remainder
    assert_eq!(page_slice(21, 0, 20), (0, 20, 0, 2));
    assert_eq!(page_slice(21, 1, 20), (20, 21, 1, 2));
    // an out-of-range page clamps to the last one (the list shrank)
    assert_eq!(page_slice(21, 9, 20), (20, 21, 1, 2));
    // a full second page ends at the list end
    assert_eq!(page_slice(40, 1, 20), (20, 40, 1, 2));
    assert_eq!(page_slice(61, 3, 20), (60, 61, 3, 4));
}

/// The pager's UI-local state (ChatUiState, like the table sorts):
/// prev/next step per (surface, list) independently, below-zero
/// clamps at the first page, the push-time clamp re-bases a stored
/// page against the list's current length (and writes it back, so
/// the next step moves from the visible page), every step bumps the
/// push generation, and a workspace switch resets everything.
#[test]
fn list_page_state_steps_clamps_and_resets() {
    let mut st = ChatUiState::default();
    st.enter_workspace("ws-a");
    let g = st.generation;
    st.page_list_by("organization", "declined", 1);
    st.page_list_by("organization", "declined", 1);
    assert_eq!(st.clamp_list_page("organization", "declined", 100), 2);
    assert_eq!(st.generation, g + 2, "every step stales in-flight pushes");
    // stepping below the first page clamps at zero
    st.page_list_by("organization", "declined", -9);
    assert_eq!(st.clamp_list_page("organization", "declined", 100), 0);
    // the clamp writes back: page 3 on a 2-page list re-bases to the
    // last page, and the next "prev" moves from THERE
    st.page_list_by("organization", "declined", 3);
    assert_eq!(st.clamp_list_page("organization", "declined", 30), 1);
    st.page_list_by("organization", "declined", -1);
    assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
    // per-(surface, list) independence
    st.page_list_by("memory", "applied", 1);
    assert_eq!(st.clamp_list_page("memory", "applied", 100), 1);
    assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
    // a workspace switch resets the pages with the rest of the state
    st.enter_workspace("ws-b");
    assert_eq!(st.clamp_list_page("memory", "applied", 100), 0);
}

/// The Shared Files nav row: shares, the screen, or a vote on record keep
/// it - an open persist on a share that just aged out stays reachable.
#[test]
fn the_files_row_stays_for_votes_on_record() {
    use crate::surfaces::files_row_visible;
    assert!(!files_row_visible(0, false, false));
    assert!(files_row_visible(1, false, false));
    assert!(files_row_visible(0, true, false));
    assert!(files_row_visible(0, false, true));
}
