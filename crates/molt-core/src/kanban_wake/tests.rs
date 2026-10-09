use super::*;
use crate::kanban_calendar::{parse_date, parse_datetime};
use crate::kanban_dates::derive;
use crate::kanban_fold::tests::{add, cs, fold, id, ids, st};
use serde_json::json;

fn t(s: &str) -> NaiveDateTime {
    parse_datetime(s).expect("fixture time")
}

fn day(s: &str) -> NaiveDate {
    parse_date(s).expect("fixture date")
}

fn actions(board: &BoardState, seat: &str, now: &str, lead: u16) -> Vec<WakeAction> {
    let now = t(now);
    task_actions(board, &derive(board, now.date()), seat, now, lead)
}

fn scan(board: &BoardState, seat: &str, now: &str, lead: u16) -> StartScan {
    let now = t(now);
    start_scan(board, &derive(board, now.date()), seat, now, lead)
}

fn start(n: u32, occurrence: Option<&str>, begins: &str, late: bool) -> WakeAction {
    WakeAction::TaskStart {
        task: id(n),
        occurrence: occurrence.map(str::to_string),
        begins: begins.to_string(),
        late,
    }
}

fn wip(n: u32) -> WakeAction {
    WakeAction::TaskWip { task: id(n) }
}

fn startable(n: u32) -> WakeAction {
    WakeAction::TaskStartable { task: id(n) }
}

/// The §5.3 board: A..E, M1 as 1..6, A in progress.
fn example() -> BoardState {
    fold(&[
        cs(vec![
            add(1, "A", &["mara"], json!({})),
            add(5, "E", &["walter"], json!({"due": "2026-10-28"})),
            add(2, "B", &["walter"], json!({"blocked_by": ids(&[1, 5])})),
            add(3, "C", &["bot"], json!({"blocked_by": ids(&[1]), "due": "2026-10-20"})),
            add(4, "D", &["mara"], json!({})),
            add(6, "M1", &["mara"], json!({"blocked_by": ids(&[1, 2, 3]), "due": "2026-10-23"})),
        ]),
        cs(vec![st(1, "wip", None)]),
    ])
}

#[test]
fn the_list_for_the_worked_example() {
    let b = example();
    let now = "2026-10-12T12:00";
    assert_eq!(actions(&b, "mara", now, 10), [wip(1), startable(4)]);
    assert_eq!(actions(&b, "walter", now, 10), [startable(5)]);
    assert!(actions(&b, "bot", now, 10).is_empty(), "C waits for A");
}

fn meeting() -> BoardState {
    fold(&[meeting_ops()])
}

#[test]
fn a_start_is_due_at_the_lead_and_only_for_assignees() {
    let b = meeting();
    let before = scan(&b, "mara", "2026-10-12T13:49", 10);
    assert!(before.due.is_empty());
    assert_eq!(before.next_fire, Some(t("2026-10-12T13:50")));

    let at = scan(&b, "mara", "2026-10-12T13:50", 10);
    assert_eq!(at.due.len(), 1);
    assert_eq!(at.due[0].key(), format!("{}@2026-10-12T14:00", id(7)));
    assert!(!at.due[0].blocked);
    assert_eq!(
        actions(&b, "mara", "2026-10-12T13:50", 10),
        [start(7, None, "2026-10-12T14:00", false)],
        "a due start is not listed a second time as startable"
    );
    assert!(scan(&b, "bot", "2026-10-12T13:50", 10).due.is_empty(), "not an assignee");
    assert_eq!(
        actions(&b, "walter", "2026-10-12T14:30", 0),
        [start(7, None, "2026-10-12T14:00", true)],
        "the start has passed"
    );
}

#[test]
fn a_start_older_than_a_day_is_no_longer_due() {
    let b = meeting();
    assert_eq!(scan(&b, "mara", "2026-10-13T13:59", 0).due.len(), 1);
    assert!(scan(&b, "mara", "2026-10-13T14:00", 0).due.is_empty());
    assert_eq!(
        actions(&b, "mara", "2026-10-13T14:00", 0),
        [startable(7)],
        "still open work of the seat"
    );
}

#[test]
fn an_all_day_task_starts_at_midnight() {
    let b = fold(&[cs(vec![add(
        8,
        "offsite",
        &["mara"],
        json!({"when": {"start": "2026-10-14", "end": "2026-10-15"}}),
    )])]);
    let s = scan(&b, "mara", "2026-10-13T23:50", 10);
    assert_eq!(s.due.len(), 1);
    assert_eq!(s.due[0].begins, t("2026-10-14T00:00"));
}

#[test]
fn every_occurrence_of_a_series_is_its_own_start() {
    let b = fold(&[cs(vec![add(
        9,
        "weekly",
        &["mara"],
        json!({
            "when": {"start": "2026-10-12T09:00", "end": "2026-10-12T09:30"},
            "repeat": {"freq": "weekly"},
            "skip": ["2026-10-19"]
        }),
    )])]);
    let first = scan(&b, "mara", "2026-10-12T09:00", 0);
    assert_eq!(first.due.len(), 1);
    assert_eq!(first.due[0].occurrence, Some(day("2026-10-12")));
    assert_eq!(
        scan(&b, "mara", "2026-10-12T10:00", 0).next_fire,
        Some(t("2026-10-26T09:00")),
        "the skipped week fires nothing"
    );
    let third = scan(&b, "mara", "2026-10-26T09:00", 0);
    assert_eq!(third.due.len(), 1);
    assert_ne!(third.due[0].key(), first.due[0].key());
    assert_eq!(
        actions(&b, "mara", "2026-10-26T09:00", 0),
        [start(9, Some("2026-10-26"), "2026-10-26T09:00", false)],
        "a series is never startable, only its occurrences start"
    );
}

#[test]
fn a_blocked_appointment_is_due_but_listed_only_once_unblocked() {
    let blocked = fold(&[cs(vec![
        add(1, "prep", &["walter"], json!({})),
        add(
            2,
            "review",
            &["mara"],
            json!({
                "blocked_by": ids(&[1]),
                "when": {"start": "2026-10-12T14:00", "end": "2026-10-12T15:00"}
            }),
        ),
    ])]);
    let s = scan(&blocked, "mara", "2026-10-12T14:00", 0);
    assert_eq!(s.due.len(), 1);
    assert!(s.due[0].blocked, "the timer consumes it without a wake");
    assert!(actions(&blocked, "mara", "2026-10-12T14:00", 0).is_empty());

    let freed = fold(&[
        cs(vec![
            add(1, "prep", &["walter"], json!({})),
            add(
                2,
                "review",
                &["mara"],
                json!({
                    "blocked_by": ids(&[1]),
                    "when": {"start": "2026-10-12T14:00", "end": "2026-10-12T15:00"}
                }),
            ),
        ]),
        cs(vec![st(1, "wip", None)]),
        cs(vec![st(1, "success", None)]),
    ]);
    assert_eq!(actions(&freed, "mara", "2026-10-12T16:00", 0), [start(2, None, "2026-10-12T14:00", true)]);
}

#[test]
fn a_closed_task_starts_nothing() {
    let b = fold(&[
        meeting_ops(),
        cs(vec![st(7, "cancelled", Some("off"))]),
    ]);
    assert!(scan(&b, "mara", "2026-10-12T14:00", 0).due.is_empty());
    assert!(scan(&b, "mara", "2026-10-12T13:00", 0).next_fire.is_none());
}

fn meeting_ops() -> serde_json::Value {
    cs(vec![add(
        7,
        "sync",
        &["mara", "walter"],
        json!({"when": {"start": "2026-10-12T14:00", "end": "2026-10-12T15:00"}}),
    )])
}

#[test]
fn the_list_entries_serialize_by_kind() {
    let v = serde_json::to_value(vec![
        start(7, None, "2026-10-12T14:00", false),
        start(9, Some("2026-10-26"), "2026-10-26T09:00", true),
        WakeAction::Vote { proposal: 3, surface: Surface::Quests, since: 5 },
        WakeAction::Poke { by: "walter".into(), since: 6 },
    ])
    .expect("json");
    assert_eq!(
        v,
        json!([
            {"kind": "task_start", "task": id(7), "begins": "2026-10-12T14:00", "late": false},
            {"kind": "task_start", "task": id(9), "occurrence": "2026-10-26", "begins": "2026-10-26T09:00", "late": true},
            {"kind": "vote", "proposal": 3, "surface": "quests", "since": 5},
            {"kind": "poke", "by": "walter", "since": 6}
        ])
    );
}

#[test]
fn a_key_carries_its_start() {
    let k = format!("{}@2026-10-12T14:00", id(7));
    assert_eq!(key_begins(&k), Some(t("2026-10-12T14:00")));
    assert_eq!(key_begins("junk"), None);
}
