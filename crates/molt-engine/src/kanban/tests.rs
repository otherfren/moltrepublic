//! S2a keystones (`docs/kanban/kanban_workflows.md` §9): both doors, the
//! precheck, the fold cache, the void marker and the card's advisories.

use molt_core::{ChainChange, EventEnvelope, MoltError, ProposalId, Reply, Surface, WorkspaceEvent};
use serde_json::{json, Value};

use crate::chain::test_support::{genesis_seat, Builder};

/// Mon 2026-10-12, 12:00 UTC.
const TODAY: u64 = 1_791_806_400;

fn tid(n: u32) -> String {
    format!("{n:08x}{}", "0".repeat(24))
}

fn cs(ops: Vec<Value>) -> Value {
    json!({"op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": ops})
}

fn add(n: u32, title: &str, extra: Value) -> Value {
    let mut v = json!({"act": "add", "id": tid(n), "creator": "mara", "title": title, "assignees": ["mara"]});
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    v
}

fn st(n: u32, to: &str, note: Option<&str>) -> Value {
    let mut v = json!({"act": "state", "id": tid(n), "to": to});
    if let Some(note) = note {
        v["note"] = json!(note);
    }
    v
}

fn republic() -> Builder {
    Builder::new_with_features(&["mara", "walter", "bot"], 2, &["quests"], false)
}

fn seat(member: &str, b: &Builder) -> crate::State {
    let mut s = genesis_seat(member, b, b.blocks.clone());
    s.presence.clock_override = Some(TODAY);
    s
}

/// Seal `payload` as applied proposal `id` on the builder's chain.
fn commit(b: &mut Builder, id: u64, payload: Value) -> molt_core::ChainBlock {
    b.commit(
        ChainChange::Applied {
            proposal_id: id,
            surface: Surface::Quests,
            payload,
        },
        &["mara", "walter"],
    )
}

fn board(s: &mut crate::State) -> Value {
    s.refresh_kanban_cache();
    s.snapshot(Surface::Quests, None, None)
        .board
        .expect("the quests read carries the board")
}

fn wire(s: &mut crate::State, seq: u64, id: u64, payload: Value) {
    let env = EventEnvelope {
        prev_seq: 0,
        seq,
        ts: TODAY,
        by: "mara".to_string(),
        body: WorkspaceEvent::Proposed {
            id: ProposalId(id),
            surface: Surface::Quests,
            payload,
        },
    };
    s.cmd_net_delivered("mara".to_string(), env, None)
        .expect("a wire drop acks, never errors");
}

#[test]
fn ref_at_or_creatorless_payloads_are_dropped_at_the_wire() {
    let b = republic();
    let mut walter = seat("walter", &b);
    let mut no_creator = add(1, "a", json!({}));
    no_creator.as_object_mut().expect("obj").remove("creator");
    let drops = [
        cs(vec![add(1, "a", json!({"ref": "a"}))]),
        cs(vec![add(1, "a", json!({})), add(2, "b", json!({"blocked_by": ["@a"]}))]),
        cs(vec![json!({"act": "state", "id": "@a", "to": "wip"})]),
        cs(vec![no_creator]),
        cs(vec![add(1, "a", json!({"creator": "eve"}))]),
        json!({"op": "add_quest", "title": "t"}),
    ];
    for (i, p) in drops.iter().enumerate() {
        let id = 100 + u64::try_from(i).expect("small");
        wire(&mut walter, id, id, p.clone());
        assert!(!walter.proposals.contains_key(&id), "{p}");
    }
    wire(&mut walter, 200, 200, cs(vec![add(1, "a", json!({}))]));
    assert!(walter.proposals.contains_key(&200), "a canonical changeset is recorded");
}

#[test]
fn the_propose_door_canonicalizes_and_overwrites_a_supplied_creator() {
    let b = republic();
    let mut walter = seat("walter", &b);
    let payload = cs(vec![
        json!({"act": "add", "ref": "api", "title": "api", "assignees": ["mara"], "creator": "mara"}),
        json!({"act": "add", "ref": "ui", "title": "ui", "assignees": ["walter"], "blocked_by": ["@api"]}),
    ]);
    let Reply::Proposed { id, minted, .. } =
        walter.cmd_propose(Surface::Quests, payload).expect("propose")
    else {
        panic!("not a Proposed reply");
    };
    assert_eq!(minted.len(), 2);
    assert!(minted.iter().all(|m| m.len() == 32), "{minted:?}");
    let recorded = &walter.proposals.get(&id.0).expect("card").payload;
    let ops = recorded["ops"].as_array().expect("ops");
    assert_eq!(ops[0]["creator"], json!("walter"), "the calling seat, whatever was supplied");
    assert_eq!(ops[1]["creator"], json!("walter"));
    assert_eq!(ops[0]["id"], json!(minted[0]));
    assert_eq!(ops[1]["blocked_by"], json!([minted[0]]));
    assert!(walter.kanban_wire_check(recorded).is_ok(), "what is signed passes the wire door");
}

#[test]
fn the_propose_door_refuses_a_bad_shape_and_what_would_void_now() {
    let mut b = republic();
    commit(
        &mut b,
        1,
        cs(vec![add(1, "a", json!({})), add(2, "b", json!({"blocked_by": [tid(1)]}))]),
    );
    let mut walter = seat("walter", &b);
    let shape = walter
        .cmd_propose(Surface::Quests, cs(vec![json!({"act": "state", "id": "@x", "to": "wip"})]))
        .expect_err("unresolved ref");
    assert!(matches!(&shape, MoltError::BadPayload(m) if m.contains("unresolved")), "{shape}");
    let void = walter
        .cmd_propose(Surface::Quests, cs(vec![st(2, "wip", None)]))
        .expect_err("blocked");
    assert!(
        matches!(&void, MoltError::BadPayload(m) if m.contains("cannot start: blocked by")),
        "{void}"
    );
    assert!(
        walter.proposals.values().all(|p| p.state != molt_core::ProposalState::Proposed),
        "nothing was minted"
    );
}

#[test]
fn a_pending_changeset_on_a_task_another_vote_cancelled_reads_would_void() {
    let mut b = republic();
    commit(&mut b, 1, cs(vec![add(1, "a", json!({}))]));
    let mut walter = seat("walter", &b);
    let mut start = cs(vec![st(1, "wip", None)]);
    start["base_rev"] = json!(1);
    let pid = crate::chain::test_support::propose(&mut walter, Surface::Quests, start);
    let card = walter.proposals.get(&pid).cloned().expect("card");
    assert!(walter.view(pid, &card).advisories.is_empty(), "applies cleanly");

    let cancel = commit(&mut b, 2, cs(vec![st(1, "cancelled", Some("dropped"))]));
    walter.receive_block(cancel);
    walter.refresh_kanban_cache();
    let card = walter.proposals.get(&pid).cloned().expect("card");
    let lines = walter.view(pid, &card).advisories;
    assert!(
        lines.contains(
            &"would void now: #00000001: cancelled to wip not allowed (proposal 2)".to_string()
        ),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("changed since proposed: ") && l.ends_with("(proposal 2)")),
        "{lines:?}"
    );
}

#[test]
fn a_voided_changeset_leaves_the_board_unchanged_on_every_node() {
    let mut b = republic();
    let first = commit(&mut b, 1, cs(vec![add(1, "a", json!({}))]));
    let dup = commit(&mut b, 2, cs(vec![add(2, "b", json!({})), add(1, "again", json!({}))]));
    let mut seats: Vec<crate::State> = ["mara", "walter", "bot"]
        .iter()
        .map(|m| seat(m, &republic()))
        .collect();
    let mut boards = Vec::new();
    for s in &mut seats {
        s.receive_block(first.clone());
        let before = board(s);
        s.receive_block(dup.clone());
        let after = board(s);
        assert_eq!(after["rev"], json!(2), "a void changeset still counts");
        assert_eq!(after["tasks"], before["tasks"], "…and changes nothing else");
        let snap = s.snapshot(Surface::Quests, None, None);
        let row = snap.accepted.iter().find(|v| v.id.0 == 2).expect("accepted row");
        assert!(
            row.void.as_deref().is_some_and(|r| r.contains("already exists")),
            "{:?}",
            row.void
        );
        let ok = snap.accepted.iter().find(|v| v.id.0 == 1).expect("accepted row");
        assert_eq!(ok.void, None);
        boards.push(after);
    }
    assert!(boards.windows(2).all(|w| w[0] == w[1]), "byte-identical on every node");
}

#[test]
fn the_fold_cache_equals_a_fresh_fold_after_every_block() {
    let mut b = republic();
    let mut walter = seat("walter", &b);
    let changesets = [
        cs(vec![add(1, "a", json!({"due": "2026-10-20"}))]),
        cs(vec![add(2, "b", json!({"blocked_by": [tid(1)]}))]),
        cs(vec![st(2, "wip", None)]),
        cs(vec![add(3, "c", json!({})), add(1, "again", json!({}))]),
        cs(vec![st(1, "wip", None)]),
    ];
    for (i, p) in changesets.into_iter().enumerate() {
        let block = commit(&mut b, 1 + u64::try_from(i).expect("small"), p);
        walter.receive_block(block);
        let warm = board(&mut walter);
        let cold = walter.fold_kanban();
        let c = walter
            .kanban_cache
            .as_ref()
            .expect("the cache is kept warm");
        assert_eq!(c.board, cold.board, "block {i}: the extension drifted");
        assert_eq!(c.void, cold.void, "block {i}");
        assert_eq!(c.rev_proposal, cold.rev_proposal, "block {i}");
        assert_eq!((c.legacy, c.chain), (cold.legacy, cold.chain), "block {i}");
        let saved = walter.kanban_cache.take();
        assert_eq!(
            walter.kanban_board_view(),
            warm,
            "block {i}: the cold read drifted"
        );
        walter.kanban_cache = saved;
    }
    let c = walter.kanban_cache.as_ref().expect("warm");
    assert_eq!(c.board.rev, 5);
    assert_eq!(
        c.void.keys().copied().collect::<Vec<_>>(),
        [3, 4],
        "the blocked start and the dup add voided"
    );
}

#[test]
fn the_reply_and_the_card_carry_the_impact() {
    // §5.3: A..E, M1 as 1..6, A in progress
    let mut b = republic();
    commit(
        &mut b,
        1,
        cs(vec![
            add(1, "A", json!({})),
            add(2, "B", json!({"blocked_by": [tid(1), tid(5)]})),
            add(3, "C", json!({"blocked_by": [tid(1)], "due": "2026-10-20"})),
            add(4, "D", json!({})),
            add(5, "E", json!({"due": "2026-10-28"})),
            add(
                6,
                "M1",
                json!({"blocked_by": [tid(1), tid(2), tid(3)], "due": "2026-10-23"}),
            ),
        ]),
    );
    commit(&mut b, 2, cs(vec![st(1, "wip", None)]));
    let mut walter = seat("walter", &b);
    let v = board(&mut walter);
    assert_eq!(v["today"], json!("2026-10-12"));
    assert_eq!(v["tasks"][tid(5)]["flags"], json!(["date_conflict"]));
    let cases = [
        (
            5,
            "2026-10-21",
            "#00000005 E: needed by 2026-10-23 → 2026-10-21 · date_conflict resolved",
        ),
        (
            6,
            "2026-10-16",
            "#00000001 A, #00000003 C: needed by 2026-10-20 → 2026-10-16 · \
             #00000002 B, #00000005 E, #00000006 M1: 2026-10-23 → 2026-10-16 · \
             #00000003 C: date_conflict (due 2026-10-20)",
        ),
    ];
    for (task, due, want) in cases {
        let mut set = cs(vec![
            json!({"act": "set", "id": tid(task), "fields": {"due": due}}),
        ]);
        set["base_rev"] = json!(2);
        let Reply::Proposed { id, warnings, .. } =
            walter.cmd_propose(Surface::Quests, set).expect("propose")
        else {
            panic!("not a Proposed reply");
        };
        assert_eq!(warnings, [want]);
        let card = walter.proposals.get(&id.0).cloned().expect("card");
        assert_eq!(walter.view(id.0, &card).advisories, [want]);
    }
}
