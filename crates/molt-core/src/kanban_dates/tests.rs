use super::*;
use crate::kanban_calendar::parse_date;
use crate::kanban_fold::tests::{add, cs, fold, id, ids, set, st};
use crate::kanban_fold::{kanban_fold_one, FoldStep};
use serde_json::{json, Value};

fn d(s: &str) -> NaiveDate {
    parse_date(s).expect("fixture date")
}

const A: u32 = 1;
const B: u32 = 2;
const C: u32 = 3;
const D: u32 = 4;
const E: u32 = 5;
const M1: u32 = 6;

/// The §5.3 example, `today` = Mon 2026-10-12.
fn example() -> BoardState {
    fold(&[
        cs(vec![
            add(A, "API schema", &["mara"], json!({"size": "M"})),
            add(
                E,
                "Vendor contract",
                &["walter"],
                json!({"due": "2026-10-28"}),
            ),
            add(
                B,
                "Client",
                &["walter"],
                json!({"size": "L", "blocked_by": ids(&[A, E])}),
            ),
            add(
                C,
                "Docs",
                &["bot"],
                json!({"size": "S", "blocked_by": ids(&[A]), "due": "2026-10-20"}),
            ),
            add(D, "Logging", &["mara"], json!({"size": "M"})),
            add(
                M1,
                "Beta",
                &["mara"],
                json!({"blocked_by": ids(&[A, B, C]), "due": "2026-10-23"}),
            ),
        ]),
        cs(vec![st(A, "wip", None)]),
    ])
}

fn today() -> NaiveDate {
    d("2026-10-12")
}

fn td(dv: &Derived, n: u32) -> &TaskDerived {
    dv.tasks.get(&id(n)).expect("derived task")
}

fn with(board: &BoardState, ops: Vec<Value>) -> BoardState {
    let mut b = board.clone();
    let seats = crate::kanban_fold::tests::seats();
    assert_eq!(kanban_fold_one(&mut b, &cs(ops), &seats), FoldStep::Applied);
    b
}

#[test]
fn the_worked_example() {
    let dv = derive(&example(), today());
    let nb = |n| td(&dv, n).needed_by;
    assert_eq!(nb(A), Some(d("2026-10-20")));
    assert_eq!(nb(B), Some(d("2026-10-23")));
    assert_eq!(nb(C), Some(d("2026-10-20")));
    assert_eq!(nb(D), None);
    assert_eq!(nb(E), Some(d("2026-10-23")));
    assert_eq!(nb(M1), Some(d("2026-10-23")));
    let conflicts: Vec<u32> = [A, B, C, D, E, M1]
        .into_iter()
        .filter(|n| td(&dv, *n).date_conflict)
        .collect();
    assert_eq!(conflicts, [E]);
    assert!(dv
        .tasks
        .values()
        .all(|t| !t.overdue && !t.stuck && !t.started_on_reopened));
    assert_eq!(td(&dv, A).shown, Shown::Wip);
    assert_eq!(td(&dv, B).shown, Shown::Blocked);
    assert_eq!(td(&dv, C).shown, Shown::Blocked);
    assert_eq!(td(&dv, D).shown, Shown::Open);
    assert_eq!(td(&dv, E).shown, Shown::Open);
    assert_eq!(td(&dv, A).prerequisite_for, [id(B), id(C), id(M1)]);
    assert_eq!(dv.priority, [id(A), id(C), id(B), id(E), id(M1), id(D)]);
    assert_eq!(dv.next.get("mara"), Some(&vec![id(A), id(D)]));
    assert_eq!(dv.next.get("walter"), Some(&vec![id(E)]));
    assert_eq!(dv.next.get("bot"), Some(&vec![]));
}

#[test]
fn impact_moving_e_due_earlier_resolves_its_conflict() {
    let before = example();
    let after = with(&before, vec![set(E, json!({"due": "2026-10-21"}))]);
    let imp = impact(&derive(&before, today()), &derive(&after, today()));
    assert_eq!(
        imp,
        [
            ImpactChange::NeededBy {
                task: id(E),
                from: Some(d("2026-10-23")),
                to: Some(d("2026-10-21"))
            },
            ImpactChange::Flag {
                task: id(E),
                flag: Flag::DateConflict,
                on: false
            },
        ]
    );
}

#[test]
fn impact_moving_the_beta_earlier_flows_backwards() {
    let before = example();
    let after = with(&before, vec![set(M1, json!({"due": "2026-10-16"}))]);
    let imp = impact(&derive(&before, today()), &derive(&after, today()));
    let to16 = |n: u32, from: &str| ImpactChange::NeededBy {
        task: id(n),
        from: Some(d(from)),
        to: Some(d("2026-10-16")),
    };
    assert_eq!(
        imp,
        [
            to16(A, "2026-10-20"),
            to16(B, "2026-10-23"),
            to16(C, "2026-10-20"),
            ImpactChange::Flag {
                task: id(C),
                flag: Flag::DateConflict,
                on: true
            },
            to16(E, "2026-10-23"),
            to16(M1, "2026-10-23"),
        ]
    );
}

#[test]
fn needed_by_through_a_diamond() {
    // top blocked by left and right, both blocked by base
    let b = fold(&[cs(vec![
        add(1, "base", &["mara"], json!({"due": "2026-12-01"})),
        add(
            2,
            "left",
            &["mara"],
            json!({"blocked_by": ids(&[1]), "due": "2026-11-15"}),
        ),
        add(3, "right", &["mara"], json!({"blocked_by": ids(&[1])})),
        add(
            4,
            "top",
            &["mara"],
            json!({"blocked_by": ids(&[2, 3]), "due": "2026-11-20"}),
        ),
    ])]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 4).needed_by, Some(d("2026-11-20")));
    assert_eq!(td(&dv, 3).needed_by, Some(d("2026-11-20")));
    assert_eq!(td(&dv, 2).needed_by, Some(d("2026-11-15")));
    assert_eq!(td(&dv, 1).needed_by, Some(d("2026-11-15")));
    assert!(td(&dv, 1).date_conflict);
    // a closed dependent no longer pulls
    let b = with(&b, vec![st(2, "cancelled", Some("dropped"))]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 1).needed_by, Some(d("2026-11-20")));
    // overdue once the day has passed
    let dv = derive(&b, d("2026-11-21"));
    assert!(td(&dv, 1).overdue && td(&dv, 4).overdue);
}

#[test]
fn stuck_propagates_until_the_failure_is_retried() {
    let b = fold(&[
        cs(vec![
            add(1, "root", &["mara"], json!({})),
            add(2, "mid", &["mara"], json!({"blocked_by": ids(&[1])})),
            add(3, "top", &["walter"], json!({"blocked_by": ids(&[2])})),
            add(4, "other", &["walter"], json!({})),
        ]),
        cs(vec![st(1, "wip", None), st(1, "fail", Some("broke"))]),
    ]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 2).shown, Shown::Stuck);
    assert_eq!(td(&dv, 3).shown, Shown::Stuck);
    assert!(td(&dv, 2).stuck && td(&dv, 3).stuck && !td(&dv, 4).stuck);
    assert_eq!(dv.next.get("walter"), Some(&vec![id(4)]));
    let b = with(&b, vec![st(1, "wip", None)]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 2).shown, Shown::Blocked);
    assert_eq!(td(&dv, 3).shown, Shown::Blocked);
    let b = with(&b, vec![set(2, json!({"blocked_by": null}))]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 2).shown, Shown::Open);
}

#[test]
fn started_on_a_reopened_prerequisite() {
    let b = fold(&[
        cs(vec![
            add(1, "p", &["mara"], json!({})),
            add(2, "q", &["mara"], json!({"blocked_by": ids(&[1])})),
        ]),
        cs(vec![
            st(1, "wip", None),
            st(1, "success", None),
            st(2, "wip", None),
        ]),
        cs(vec![st(1, "todo", Some("not done after all"))]),
    ]);
    let dv = derive(&b, today());
    assert!(td(&dv, 2).started_on_reopened);
    assert_eq!(td(&dv, 2).shown, Shown::Wip);
    assert!(dv.next["mara"].contains(&id(2)));
}

#[test]
fn scheduled_later_and_timed_conflicts() {
    let b = fold(&[cs(vec![
        add(
            1,
            "prep",
            &["mara"],
            json!({"when": {"start": "2026-10-14T09:00", "end": "2026-10-14T12:00"}}),
        ),
        add(
            2,
            "talk",
            &["mara"],
            json!({"blocked_by": ids(&[1]),
            "when": {"start": "2026-10-14T11:00", "end": "2026-10-14T12:00"}}),
        ),
        add(3, "later", &["bot"], json!({"after": "2026-10-20"})),
        add(
            4,
            "sync",
            &["bot"],
            json!({"when": {"start": "2026-10-12", "end": "2026-10-12"}, "repeat": {"freq": "daily"}}),
        ),
    ])]);
    let dv = derive(&b, today());
    assert_eq!(td(&dv, 1).shown, Shown::Scheduled);
    assert_eq!(td(&dv, 2).shown, Shown::Blocked);
    assert_eq!(td(&dv, 3).shown, Shown::Later);
    assert_eq!(td(&dv, 4).shown, Shown::Scheduled);
    assert!(td(&dv, 1).date_conflict, "prep ends after the talk starts");
    assert!(!td(&dv, 2).date_conflict);
    // not startable yet, and a series is never in next
    assert_eq!(dv.next.get("bot"), Some(&vec![]));
    assert_eq!(dv.next.get("mara"), Some(&vec![id(1)]));
    let dv = derive(&b, d("2026-10-20"));
    assert_eq!(dv.next.get("bot"), Some(&vec![id(3)]));
}

#[test]
fn derive_is_a_pure_function_of_board_and_day() {
    let b = example();
    assert_eq!(derive(&b, today()), derive(&b.clone(), today()));
    assert!(impact(&derive(&b, today()), &derive(&b, today())).is_empty());
}
