// SPDX-License-Identifier: GPL-3.0-or-later
//! The chat bus's channel projection and the proposal cache.

use super::*;

#[test]
fn derive_channels_lists_only_open_vote_discussions() {
    let known_of = |title: &str, fate: KnownFate| KnownProposal {
        payload: serde_json::json!({"op": "add_note", "title": title}),
        surface: Surface::Memory,
        approvals: 1,
        threshold: 2,
        fate,
    };
    let infos = vec![
        ChannelInfo {
            channel: ChannelRef::Topic { name: "zeta".into() },
            count: 4,
            last_ts: 40,
            state: None,
            unread: 0,
        },
        ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(7) },
            count: 1,
            last_ts: 30,
            state: None,
            unread: 0,
        },
        ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(5) },
            count: 2,
            last_ts: 20,
            state: Some(ProposalState::Applied),
            unread: 0,
        },
        ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(3) },
            count: 5,
            last_ts: 10,
            state: Some(ProposalState::Proposed),
            unread: 0,
        },
        ChannelInfo {
            channel: ChannelRef::Group,
            count: 9,
            last_ts: 50,
            state: None,
            unread: 0,
        },
    ];
    let known = HashMap::from([
        (3u64, known_of("raise budget", KnownFate::Pending)),
        (5u64, known_of("sealed one", KnownFate::Applied)),
    ]);
    let unread = HashMap::from([("patch:3".to_string(), 2usize), ("group".to_string(), 1)]);
    let rows = derive_channels(0, &infos, &known, &unread);
    // topics first (a human named them), then the discussions of OPEN
    // votes. No group row - the Gruppe nav view covers it - and no
    // sealed/closed votes or unknown proposals: a discussion is
    // vote-bound and dies with its vote.
    //
    // The TOPIC row is the one this list lost once, and losing it made
    // the New-topic button a trapdoor: the channel existed and held
    // messages with nowhere to click back to.
    assert_eq!(
        rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        ["topic:zeta", "patch:3"],
        "the topic keeps its row; only the open vote's discussion survives"
    );
    assert_eq!(rows[0].label, "zeta", "a topic is labelled by its name");
    assert_eq!(rows[1].label, "raise budget", "patch title from proposal state");
    assert_eq!(rows[1].unread, 2);
    // nothing open → no rows (the sidebar hides the whole section)
    let rows = derive_channels(0, &[], &HashMap::new(), &HashMap::new());
    assert!(rows.is_empty());
}

#[test]
fn vote_jump_targets_the_hosting_surface_and_fate_view() {
    let known_of = |surface: Surface, fate: KnownFate| KnownProposal {
        payload: serde_json::json!({"op": "add_note", "title": "t"}),
        surface,
        approvals: 0,
        threshold: 2,
        fate,
    };
    let known = HashMap::from([
        (5u64, known_of(Surface::Organization, KnownFate::Pending)),
        (6u64, known_of(Surface::Organization, KnownFate::Closed)),
        (7u64, known_of(Surface::Memory, KnownFate::Pending)),
    ]);
    // only a patch channel has a vote to jump back to
    assert!(vote_jump_command(&ChannelRef::Group, &known).is_none());
    let topic = ChannelRef::Topic { name: "zeta".to_string() };
    assert!(vote_jump_command(&topic, &known).is_none());
    // an open Organization vote → its card sits in the pending view
    assert!(matches!(
        vote_jump_command(&ChannelRef::Patch { id: ProposalId(5) }, &known),
        Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
    ));
    // a closed one moved to the declined view
    assert!(matches!(
        vote_jump_command(&ChannelRef::Patch { id: ProposalId(6) }, &known),
        Some(Command::SelectView { surface: Surface::Organization, view }) if view == "declined"
    ));
    // a gated surface hosts its cards on its main view — plain surface
    // selection, exactly like the sidebar row
    assert!(matches!(
        vote_jump_command(&ChannelRef::Patch { id: ProposalId(7) }, &known),
        Some(Command::SelectSurface { surface: Surface::Memory })
    ));
    // a cache miss (this UI never saw the proposal) falls back to the
    // Organization pending view — never a dead button
    assert!(matches!(
        vote_jump_command(&ChannelRef::Patch { id: ProposalId(99) }, &known),
        Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
    ));
    // WP1: an APPLIED Organization vote's row lives in the accepted view
    let known = HashMap::from([(8u64, {
        let mut k = known_of(Surface::Organization, KnownFate::Applied);
        k.approvals = 2;
        k
    })]);
    assert!(matches!(
        vote_jump_command(&ChannelRef::Patch { id: ProposalId(8) }, &known),
        Some(Command::SelectView { surface: Surface::Organization, view }) if view == "accepted"
    ));
}

/// Review finding: the read contract's `pending` is Proposed-only, so
/// the moment a proposal seals (or closes) it vanishes from every read
/// and the patch channel degraded to "#id" with no state line. The
/// UI-side cache must keep the title and resolve the fate from the
/// applied log the UI already reads.
#[test]
fn patch_title_and_state_survive_the_proposal_leaving_pending() {
    let pv = ProposalView {
        id: ProposalId(4),
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
        approvals: 2,
        threshold: 3,
        state: ProposalState::Proposed,
        approved_by_me: false,
        declined_by_me: false,
        current: String::new(),
        proposed: String::new(),
        votes: Vec::new(),
        declined_at: 0,
        declined_by: String::new(),
        by: String::new(),
        mine: false,
        superseded: false,
        withdrawn: false,
    };
    let mut known = HashMap::new();
    // while pending: cached with title + progress
    update_known_proposals(&mut known, std::slice::from_ref(&pv), &[], &HashMap::new());
    assert_eq!(display_title(0, &known[&4].payload), "budget", "human title, no op-code prefix");
    assert_eq!(known[&4].fate, KnownFate::Pending);

    // the proposal leaves the Proposed-only window and its payload
    // shows up in the surface's applied log → Applied
    let applied = HashMap::from([(Surface::Memory, vec![pv.payload.clone()])]);
    update_known_proposals(&mut known, &[], &[], &applied);
    assert_eq!(known[&4].fate, KnownFate::Applied);

    // the system line keeps the title and renders the sealed state
    let first_seen = HashMap::from([(4u64, 150u64)]);
    let sys = patch_system_lines(0, 4, &[], &known, &first_seen);
    let text = &sys[0].1.text;
    assert!(text.contains("budget") && text.contains('✓'), "{text}");
    assert!(text.contains("3/3"), "sealed at the threshold: {text}");

    // a sealed vote's discussion leaves the sidebar (discussions exist
    // to decide something — once decided there is nothing to vote on)
    let infos = vec![ChannelInfo {
        channel: ChannelRef::Patch { id: ProposalId(4) },
        count: 1,
        last_ts: 10,
        state: None,
        unread: 0,
    }];
    let rows = derive_channels(0, &infos, &known, &HashMap::new());
    assert!(rows.is_empty(), "an Applied vote's discussion is hidden");

    // vanished WITHOUT an applied trace: the read contract cannot tell
    // Rejected from expired — neutral closed marker, title kept, no
    // fabricated verdict
    let pv9 = ProposalView {
        id: ProposalId(9),
        payload: serde_json::json!({ "title": "drop the fee" }),
        ..pv.clone()
    };
    update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &applied);
    update_known_proposals(&mut known, &[], &[], &applied);
    assert_eq!(known[&9].fate, KnownFate::Closed);
    let sys = patch_system_lines(0, 9, &[], &known, &first_seen);
    let text = &sys[0].1.text;
    assert!(text.contains("drop the fee") && text.contains('⊘'), "{text}");
    assert!(!text.contains('✓') && !text.contains('✗'), "{text}");

    // an id never seen anywhere still tolerates (concept Q4)
    let sys = patch_system_lines(0, 77, &[], &known, &first_seen);
    assert_eq!(sys[0].1.text, "⚖ #77");

    // a Closed verdict corrects itself when the applied value shows up
    // in a later read (an out-of-order pass must not stick a wrong fate)
    let applied9 = HashMap::from([(
        Surface::Memory,
        vec![serde_json::json!({ "title": "drop the fee" })],
    )]);
    update_known_proposals(&mut known, &[], &[], &applied9);
    assert_eq!(known[&9].fate, KnownFate::Applied);
    // … while an already-Applied fate is sticky even if the surface
    // read is missing this pass
    update_known_proposals(&mut known, &[], &[], &HashMap::new());
    assert_eq!(known[&4].fate, KnownFate::Applied);
    assert_eq!(known[&9].fate, KnownFate::Applied);
}

/// One `ProposalView` for the cache tests, minimal noise.
pub(super) fn view_of(id: u64, title: &str, state: ProposalState) -> ProposalView {
    ProposalView {
        id: ProposalId(id),
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "title": title }),
        approvals: 0,
        threshold: 3,
        state,
        approved_by_me: false,
        declined_by_me: false,
        current: String::new(),
        proposed: String::new(),
        votes: Vec::new(),
        declined_at: if state == ProposalState::Rejected { 100 } else { 0 },
        declined_by: if state == ProposalState::Rejected {
            "ashi".to_string()
        } else {
            String::new()
        },
        by: String::new(),
        mine: false,
        superseded: false,
        withdrawn: false,
    }
}

/// The snapshots' `declined` lists fold into the proposal cache: a veto
/// this UI never saw pending (fresh open, another member's decline)
/// still titles its discussion channel and flags it closed — and an
/// Applied fate is never downgraded by the fold.
#[test]
fn declined_votes_fold_into_the_cache_as_closed() {
    let mut known = HashMap::new();
    // never seen pending: the decline inserts a Closed entry, titled
    let dv7 = view_of(7, "vetoed", ProposalState::Rejected);
    update_known_proposals(&mut known, &[], std::slice::from_ref(&dv7), &HashMap::new());
    assert_eq!(known[&7].fate, KnownFate::Closed);
    assert_eq!(display_title(0, &known[&7].payload), "vetoed", "human title from the summary");

    // a cached Pending refreshes to Closed when its decline shows up
    let pv8 = view_of(8, "late veto", ProposalState::Proposed);
    update_known_proposals(&mut known, std::slice::from_ref(&pv8), &[], &HashMap::new());
    assert_eq!(known[&8].fate, KnownFate::Pending);
    let dv8 = view_of(8, "late veto", ProposalState::Rejected);
    update_known_proposals(&mut known, &[], std::slice::from_ref(&dv8), &HashMap::new());
    assert_eq!(known[&8].fate, KnownFate::Closed);

    // an Applied fate is sticky against the fold (the applied-log probe
    // proved the seal; byte-identical-twin ambiguity must not un-seal)
    let pv9 = view_of(9, "sealed", ProposalState::Proposed);
    update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &HashMap::new());
    let applied = HashMap::from([(Surface::Memory, vec![pv9.payload.clone()])]);
    update_known_proposals(&mut known, &[], &[], &applied);
    assert_eq!(known[&9].fate, KnownFate::Applied);
    let dv9 = view_of(9, "sealed", ProposalState::Rejected);
    update_known_proposals(&mut known, &[], std::slice::from_ref(&dv9), &applied);
    assert_eq!(known[&9].fate, KnownFate::Applied, "never downgraded");

    // …and the derive_channels contract holds over the folded cache:
    // the closed discussion stays OFF the sidebar
    let infos = vec![ChannelInfo {
        channel: ChannelRef::Patch { id: ProposalId(7) },
        count: 2,
        last_ts: 20,
        state: Some(ProposalState::Rejected),
        unread: 0,
    }];
    assert!(
        derive_channels(0, &infos, &known, &HashMap::new()).is_empty(),
        "a declined vote's discussion is not a sidebar row"
    );
}

/// The decision-panel flag: only an ORGANIZATION decision's discussion.
///
/// The ask is explicit that other surfaces' decisions are handled
/// differently, so the panel must not appear for them. And it must not
/// appear for the group chat or a free topic either — there is no
/// decision to head those with.
#[test]
fn selected_channel_org_flags_only_organization_decisions() {
    let known_of = |surface: Surface| KnownProposal {
        payload: serde_json::json!({"op": "set_name", "value": "x"}),
        surface,
        approvals: 1,
        threshold: 2,
        fate: KnownFate::Pending,
    };
    let known = HashMap::from([
        (1u64, known_of(Surface::Organization)),
        (2u64, known_of(Surface::Memory)),
    ]);
    let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };

    assert!(selected_channel_org(&patch(1), &known), "an Organization decision");
    assert!(
        !selected_channel_org(&patch(2), &known),
        "another surface's decision is handled differently - no panel"
    );
    assert!(
        !selected_channel_org(&patch(9), &known),
        "an unknown referent heads nothing"
    );
    assert!(!selected_channel_org(&ChannelRef::Group, &known));
    assert!(!selected_channel_org(
        &ChannelRef::Topic { name: "budget".into() },
        &known
    ));
}

/// The compose-collapse flag: only a DECIDED vote's patch channel is
/// read-only. The engine's enumeration annotation is authoritative when
/// present; otherwise the proposal cache decides; group/topic, open
/// votes and unknown referents (Q4) stay writable.
#[test]
fn selected_channel_closed_flags_only_decided_patch_votes() {
    let known_of = |fate: KnownFate| KnownProposal {
        payload: serde_json::json!({"op": "add_note", "title": "t"}),
        surface: Surface::Memory,
        approvals: 1,
        threshold: 2,
        fate,
    };
    let info = |id: u64, state: Option<ProposalState>| ChannelInfo {
        channel: ChannelRef::Patch { id: ProposalId(id) },
        count: 1,
        last_ts: 10,
        state,
        unread: 0,
    };
    let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };
    let known = HashMap::from([
        (1u64, known_of(KnownFate::Pending)),
        (2u64, known_of(KnownFate::Closed)),
        (3u64, known_of(KnownFate::Applied)),
    ]);

    // group/topic are never closed
    assert!(!selected_channel_closed(&ChannelRef::Group, &[], &known));
    assert!(!selected_channel_closed(
        &ChannelRef::Topic { name: "x".into() },
        &[],
        &known
    ));

    // the engine annotation decides when present …
    let infos = vec![
        info(1, Some(ProposalState::Proposed)),
        info(2, Some(ProposalState::Rejected)),
        info(3, Some(ProposalState::Applied)),
    ];
    assert!(!selected_channel_closed(&patch(1), &infos, &HashMap::new()));
    assert!(selected_channel_closed(&patch(2), &infos, &HashMap::new()));
    assert!(selected_channel_closed(&patch(3), &infos, &HashMap::new()));
    // … and wins over a stale cache
    let stale = HashMap::from([(2u64, known_of(KnownFate::Pending))]);
    assert!(selected_channel_closed(&patch(2), &infos, &stale));

    // no (or unannotated) enumeration entry → the cache decides — the
    // instant-feedback path on selection passes no infos at all
    assert!(!selected_channel_closed(&patch(1), &[], &known));
    assert!(selected_channel_closed(&patch(2), &[], &known));
    assert!(selected_channel_closed(&patch(3), &[], &known));
    assert!(selected_channel_closed(&patch(2), &[info(2, None)], &known));

    // unknown everywhere stays writable (chat-bus Q4)
    assert!(!selected_channel_closed(&patch(99), &infos, &known));
}

#[test]
fn channel_keys_round_trip() {
    for c in [
        ChannelRef::Group,
        ChannelRef::Patch { id: ProposalId(42) },
        ChannelRef::Topic { name: "Budget 2026".into() },
    ] {
        assert_eq!(parse_channel_key(&channel_key(&c)), Some(c));
    }
    assert_eq!(parse_channel_key("patch:xyz"), None, "junk never panics");
    assert_eq!(parse_channel_key(""), None);
}
