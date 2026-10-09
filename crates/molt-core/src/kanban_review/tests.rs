use super::*;
use crate::kanban_calendar::parse_date;
use crate::kanban_fold::tests::{add, cs, fold, id, ids, seats, set, st};
use crate::kanban_fold::{kanban_fold_one, short_id};
use serde_json::json;

fn d(s: &str) -> NaiveDate {
    parse_date(s).expect("fixture date")
}

fn today() -> NaiveDate {
    d("2026-10-12")
}

fn with_base(mut p: Value, base_rev: u64) -> Value {
    p["base_rev"] = json!(base_rev);
    p
}

fn lines(board: &BoardState, payload: &Value, by_rev: &dyn Fn(u64) -> Option<u64>) -> Vec<String> {
    let before = derive(board, today());
    advisories(board, &before, payload, &seats(), today(), by_rev)
}

fn none(_: u64) -> Option<u64> {
    None
}

#[test]
fn a_changeset_on_a_task_another_vote_cancelled_would_void() {
    let mut board = fold(&[cs(vec![add(1, "a", &["mara"], json!({}))])]);
    let pending = with_base(cs(vec![st(1, "wip", None)]), 1);
    assert!(lines(&board, &pending, &none).is_empty(), "applies cleanly today");
    kanban_fold_one(&mut board, &cs(vec![st(1, "cancelled", Some("dropped"))]), &seats());
    let got = lines(&board, &pending, &|rev| (rev == 2).then_some(41));
    assert!(
        got.contains(&format!(
            "would void now: {}: cancelled to wip not allowed (proposal 41)",
            short_id(&id(1))
        )),
        "{got:?}"
    );
    assert!(
        got.contains(&format!("changed since proposed: {} state (proposal 41)", short_id(&id(1)))),
        "{got:?}"
    );
}

#[test]
fn every_act_whose_task_moved_past_base_rev_is_named_once() {
    let board = fold(&[
        cs(vec![add(1, "a", &["mara"], json!({})), add(2, "b", &["mara"], json!({}))]),
        cs(vec![set(1, json!({"size": "S"}))]),
    ]);
    let pending = with_base(
        cs(vec![
            set(1, json!({"due": "2026-12-01", "size": "M"})),
            st(1, "wip", None),
            set(2, json!({"size": "L"})),
        ]),
        1,
    );
    let got = lines(&board, &pending, &none);
    let changed: Vec<&String> = got.iter().filter(|l| l.starts_with("changed")).collect();
    assert_eq!(
        changed,
        [&format!("changed since proposed: {} due, size, state", short_id(&id(1)))],
        "task 2 did not move since rev 1"
    );
}

#[test]
fn a_blocked_start_names_the_vote_that_reopened_the_prerequisite() {
    let mut board = fold(&[
        cs(vec![
            add(1, "a", &["mara"], json!({})),
            add(2, "b", &["mara"], json!({"blocked_by": ids(&[1])})),
        ]),
        cs(vec![st(1, "wip", None)]),
        cs(vec![st(1, "success", None)]),
    ]);
    let pending = with_base(cs(vec![st(2, "wip", None)]), 3);
    let by_rev = |rev: u64| (rev == 4).then_some(7);
    assert!(
        lines(&board, &pending, &by_rev).is_empty(),
        "applies cleanly today"
    );
    kanban_fold_one(&mut board, &cs(vec![st(1, "todo", None)]), &seats());
    let got = lines(&board, &pending, &by_rev);
    assert_eq!(
        got.first(),
        Some(&format!(
            "would void now: {} cannot start: blocked by {} (todo) (proposal 7)",
            short_id(&id(2)),
            short_id(&id(1))
        )),
        "{got:?}"
    );
}

#[test]
fn a_void_on_a_task_untouched_since_base_rev_names_no_vote() {
    let board = fold(&[cs(vec![add(1, "a", &["mara"], json!({}))])]);
    let pending = with_base(cs(vec![st(1, "success", None)]), 1);
    let got = lines(&board, &pending, &|_| Some(41));
    assert_eq!(
        got,
        [format!(
            "would void now: {} cannot success: not started",
            short_id(&id(1))
        )]
    );
}

/// The §5.3 worked example: A..E, M1 as ids 1..6, A in progress.
fn worked_example() -> BoardState {
    fold(&[
        cs(vec![
            add(1, "API schema", &["mara"], json!({})),
            add(
                2,
                "Client",
                &["walter"],
                json!({"blocked_by": ids(&[1, 5])}),
            ),
            add(
                3,
                "Docs",
                &["bot"],
                json!({"blocked_by": ids(&[1]), "due": "2026-10-20"}),
            ),
            add(4, "Logging", &["mara"], json!({})),
            add(
                5,
                "Vendor contract",
                &["walter"],
                json!({"due": "2026-10-28"}),
            ),
            add(
                6,
                "Beta",
                &["mara"],
                json!({"blocked_by": ids(&[1, 2, 3]), "due": "2026-10-23"}),
            ),
        ]),
        cs(vec![st(1, "wip", None)]),
    ])
}

#[test]
fn the_impact_renders_as_one_line_grouped_by_change() {
    let board = worked_example();
    let pending = with_base(cs(vec![set(5, json!({"due": "2026-10-21"}))]), 2);
    assert_eq!(
        lines(&board, &pending, &none),
        [format!(
            "{} Vendor contract: needed by 2026-10-23 → 2026-10-21 · date_conflict resolved",
            short_id(&id(5))
        )]
    );

    let pending = with_base(cs(vec![set(6, json!({"due": "2026-10-16"}))]), 2);
    let s = |n: u32| short_id(&id(n));
    assert_eq!(
        lines(&board, &pending, &none),
        [format!(
            "{} API schema, {} Docs: needed by 2026-10-20 → 2026-10-16 · \
             {} Client, {} Vendor contract, {} Beta: 2026-10-23 → 2026-10-16 · \
             {} Docs: date_conflict (due 2026-10-20)",
            s(1),
            s(3),
            s(2),
            s(5),
            s(6),
            s(3)
        )]
    );
}

#[test]
fn missing_content_and_unmatched_evidence_are_flagged() {
    let board = fold(&[cs(vec![add(
        1,
        "a",
        &["mara"],
        json!({"acceptance": ["fast", "documented"], "description": "x"}),
    )])]);
    let start = cs(vec![st(1, "wip", None)]);
    let mut wip = board.clone();
    kanban_fold_one(&mut wip, &start, &seats());
    let succeed = with_base(
        cs(vec![json!({"act": "state", "id": id(1), "to": "success", "evidence": ["2 s", ""]})]),
        2,
    );
    let got = lines(&wip, &succeed, &none);
    assert_eq!(got, [format!("{}: evidence for 1 of 2 criteria", short_id(&id(1)))]);

    let floating = with_base(cs(vec![add(2, "b", &["mara"], json!({}))]), 1);
    let got = lines(&board, &floating, &none);
    assert_eq!(
        got,
        [
            format!("{}: no description", short_id(&id(2))),
            format!("{}: no acceptance criteria", short_id(&id(2))),
        ]
    );
    let timed = with_base(
        cs(vec![add(2, "sync", &["mara"], json!({"when": {"start": "2026-10-20", "end": "2026-10-20"}}))]),
        1,
    );
    assert!(lines(&board, &timed, &none).is_empty(), "a timed task needs neither");
}

#[test]
fn a_payload_that_is_no_kanban_changeset_has_no_advisories() {
    let board = fold(&[]);
    assert!(lines(&board, &json!({"op": "add_quest"}), &none).is_empty());
}

#[test]
fn the_board_view_carries_tasks_with_their_derived_status() {
    let board = fold(&[cs(vec![
        add(1, "a", &["mara"], json!({"due": "2026-10-01"})),
        add(2, "b", &["walter"], json!({"blocked_by": ids(&[1])})),
    ])]);
    let dv = derive(&board, today());
    let now = today().and_hms_opt(12, 0, 0).expect("noon");
    let v = board_view(&board, &dv, now, "mara");
    assert_eq!(v["rev"], json!(1));
    assert_eq!(v["today"], json!("2026-10-12"));
    assert_eq!(v["now"], json!("2026-10-12T12:00"));
    assert_eq!(v["me"], json!("mara"));
    let a = &v["tasks"][id(1)];
    assert_eq!(a["title"], json!("a"));
    assert_eq!(a["shown"], json!("open"));
    assert_eq!(a["needed_by"], json!("2026-10-01"));
    assert_eq!(a["flags"], json!(["overdue"]));
    assert_eq!(a["prerequisite_for"], json!([id(2)]));
    assert_eq!(v["tasks"][id(2)]["shown"], json!("blocked"));
    assert_eq!(v["priority"], json!([id(1), id(2)]));
    assert_eq!(v["next"]["mara"], json!([id(1)]));
    assert_eq!(v["next"]["walter"], json!([]));
}

#[test]
fn every_act_renders_each_named_field_on_its_own_line() {
    let board = fold(&[cs(vec![add(1, "a", &["mara"], json!({"size": "S"}))])]);
    let p = cs(vec![
        set(1, json!({"assignees": ["walter", "bot"], "due": "2026-12-01", "size": null})),
        st(1, "wip", None),
        add(2, "b", &["bot"], json!({"blocked_by": ids(&[1])})),
        st(2, "cancelled", Some("not needed")),
    ]);
    let (s1, s2) = (short_id(&id(1)), short_id(&id(2)));
    assert_eq!(
        render_acts(Some(&board), &p),
        [
            format!("{s1} assignees: mara → walter, bot"),
            format!("{s1} due: none → 2026-12-01"),
            format!("{s1} size: S → none"),
            format!("{s1} state: todo → wip"),
            format!("{s2} add: b"),
            format!("{s2} assignees: bot"),
            format!("{s2} blocked_by: {s1}"),
            format!("{s2} creator: mara"),
            format!("{s2} state: todo → cancelled"),
            format!("{s2} note: not needed"),
        ]
    );
    assert_eq!(
        render_acts(None, &p)[..4],
        [
            format!("{s1} assignees: walter, bot"),
            format!("{s1} due: 2026-12-01"),
            format!("{s1} size: none"),
            format!("{s1} state: wip"),
        ]
    );
    assert!(render_acts(Some(&board), &json!({"op": "add_quest"})).is_empty());
}
