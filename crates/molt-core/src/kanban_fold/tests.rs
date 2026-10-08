use super::*;
use serde_json::json;

pub(crate) fn id(n: u32) -> String {
    format!("{n:032x}")
}

pub(crate) fn seats() -> BTreeSet<String> {
    ["mara", "walter", "bot"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub(crate) fn cs(ops: Vec<Value>) -> Value {
    json!({"op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": ops})
}

/// A canonical `add` with `extra` fields merged in.
pub(crate) fn add(n: u32, title: &str, assignees: &[&str], extra: Value) -> Value {
    let mut v = json!({"act": "add", "id": id(n), "creator": "mara", "title": title, "assignees": assignees});
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            m.insert(k.clone(), x.clone());
        }
    }
    v
}

pub(crate) fn set(n: u32, fields: Value) -> Value {
    json!({"act": "set", "id": id(n), "fields": fields})
}

pub(crate) fn st(n: u32, to: &str, note: Option<&str>) -> Value {
    let mut v = json!({"act": "state", "id": id(n), "to": to});
    if let (Some(m), Some(note)) = (v.as_object_mut(), note) {
        m.insert("note".into(), Value::from(note));
    }
    v
}

pub(crate) fn ids(list: &[u32]) -> Value {
    Value::Array(list.iter().map(|n| Value::from(id(*n))).collect())
}

pub(crate) fn fold(changesets: &[Value]) -> BoardState {
    kanban_fold(changesets, &seats())
}

fn step(board: &mut BoardState, ops: Vec<Value>) -> FoldStep {
    kanban_fold_one(board, &cs(ops), &seats())
}

fn applied(board: &mut BoardState, ops: Vec<Value>) {
    let s = step(board, ops);
    assert_eq!(s, FoldStep::Applied);
}

fn void(board: &mut BoardState, ops: Vec<Value>) -> String {
    let before = board.clone();
    match step(board, ops) {
        FoldStep::Void(r) => {
            assert_eq!(board.rev, before.rev + 1, "a void changeset still counts");
            assert_eq!(
                board.tasks, before.tasks,
                "a void changeset changes nothing"
            );
            r.0
        }
        other => panic!("expected void, got {other:?}"),
    }
}

fn state_of(board: &BoardState, n: u32) -> TaskState {
    board.tasks.get(&id(n)).expect("task").state
}

fn history() -> Vec<Value> {
    vec![
        cs(vec![
            add(1, "schema", &["mara"], json!({"size": "M"})),
            add(2, "client", &["walter"], json!({"blocked_by": ids(&[1])})),
        ]),
        cs(vec![st(1, "wip", None)]),
        // void: client is still blocked
        cs(vec![st(2, "wip", None)]),
        json!({"op": "add_quest", "value": "legacy"}),
        cs(vec![st(1, "success", None), st(2, "wip", None)]),
        cs(vec![set(
            2,
            json!({"due": "2026-11-01", "assignees": ["walter", "bot"]}),
        )]),
        cs(vec![add(
            3,
            "sync",
            &["mara", "bot"],
            json!({
            "when": {"start": "2026-10-05T09:00", "end": "2026-10-05T09:30"},
            "repeat": {"freq": "weekly"}, "skip": ["2026-10-12"]}),
        )]),
    ]
}

#[test]
fn fold_is_deterministic_one_by_one_all_at_once_and_from_a_cached_prefix() {
    let h = history();
    let all = fold(&h);
    let mut one = BoardState::default();
    for p in &h {
        kanban_fold_one(&mut one, p, &seats());
    }
    assert_eq!(one, all);
    for cut in 0..=h.len() {
        let mut cached = fold(&h[..cut]);
        kanban_fold_onto(&mut cached, &h[cut..], &seats());
        assert_eq!(cached, all, "prefix {cut}");
        assert_eq!(
            serde_json::to_string(&cached.to_json()).expect("json"),
            serde_json::to_string(&all.to_json()).expect("json")
        );
    }
    // six changesets (one void), the legacy op skipped
    assert_eq!(all.rev, 6);
    assert_eq!(state_of(&all, 2), TaskState::Wip);
    assert_eq!(all.tasks[&id(2)].touched_rev, 5);
    assert_eq!(all.tasks[&id(1)].touched_rev, 4);
}

#[test]
fn void_is_all_or_nothing_including_an_in_changeset_duplicate_id() {
    let mut b = BoardState::default();
    let r = void(
        &mut b,
        vec![
            add(1, "a", &["mara"], json!({})),
            add(2, "b", &["bot"], json!({})),
            add(1, "c", &["bot"], json!({})),
        ],
    );
    assert!(r.contains("already exists"), "{r}");
    assert!(b.tasks.is_empty());
    applied(&mut b, vec![add(1, "a", &["mara"], json!({}))]);
    let r = void(
        &mut b,
        vec![
            set(1, json!({"title": "renamed"})),
            add(1, "again", &["mara"], json!({})),
        ],
    );
    assert!(r.contains("already exists"), "{r}");
    assert_eq!(b.tasks[&id(1)].title, "a");
}

#[test]
fn non_kanban_payloads_are_skipped_without_a_rev() {
    let mut b = BoardState::default();
    assert_eq!(
        kanban_fold_one(&mut b, &json!({"op": "add_quest", "value": "x"}), &seats()),
        FoldStep::Skipped
    );
    assert_eq!(b.rev, 0);
    // a kanban changeset that is not canonical is void, deterministically
    let r = void(
        &mut b,
        vec![json!({"act": "add", "ref": "x", "title": "t", "assignees": ["mara"]})],
    );
    assert!(!r.is_empty());
}

/// Drive a fresh task to `from`; `timed` makes it an appointment.
fn task_in(from: TaskState, timed: bool) -> BoardState {
    let extra = if timed {
        json!({"when": {"start": "2026-10-12T09:00", "end": "2026-10-12T10:00"}})
    } else {
        json!({})
    };
    let mut b = BoardState::default();
    applied(&mut b, vec![add(1, "t", &["mara"], extra)]);
    let path: &[(&str, Option<&str>)] = match from {
        TaskState::Todo => &[],
        TaskState::Wip => &[("wip", None)],
        TaskState::Success => &[("wip", None), ("success", None)],
        TaskState::Fail => &[("wip", None), ("fail", Some("n"))],
        TaskState::Cancelled => &[("cancelled", Some("n"))],
    };
    for (to, note) in path {
        applied(&mut b, vec![st(1, to, *note)]);
    }
    assert_eq!(state_of(&b, 1), from);
    b
}

#[test]
fn every_row_of_the_transition_table_legal_and_illegal() {
    use TaskState::{Cancelled, Fail, Success, Todo, Wip};
    let legal = [
        (Todo, Wip),
        (Wip, Todo),
        (Wip, Success),
        (Wip, Fail),
        (Fail, Wip),
        (Todo, Cancelled),
        (Wip, Cancelled),
        (Fail, Cancelled),
        (Success, Todo),
        (Fail, Todo),
        (Cancelled, Todo),
    ];
    let appointment_only = [(Todo, Success), (Todo, Fail)];
    for timed in [false, true] {
        for from in TaskState::ALL {
            for to in TaskState::ALL {
                let mut b = task_in(from, timed);
                let ok = legal.contains(&(from, to))
                    || (timed && appointment_only.contains(&(from, to)));
                let s = step(&mut b, vec![st(1, to.as_str(), Some("n"))]);
                if ok {
                    assert_eq!(s, FoldStep::Applied, "{from:?} -> {to:?} timed={timed}");
                    assert_eq!(state_of(&b, 1), to);
                    assert_eq!(b.tasks[&id(1)].note.as_deref(), Some("n"));
                } else {
                    assert!(
                        matches!(s, FoldStep::Void(_)),
                        "{from:?} -> {to:?} timed={timed}"
                    );
                    assert_eq!(state_of(&b, 1), from);
                }
            }
        }
    }
}

#[test]
fn fail_and_cancel_require_a_note() {
    for (from, to, timed) in [
        (TaskState::Wip, "fail", false),
        (TaskState::Todo, "cancelled", false),
        (TaskState::Wip, "cancelled", false),
        (TaskState::Fail, "cancelled", false),
        (TaskState::Todo, "fail", true),
    ] {
        let mut b = task_in(from, timed);
        let r = void(&mut b, vec![st(1, to, None)]);
        assert!(r.contains("note required"), "{r}");
        let r = void(&mut b, vec![st(1, to, Some("  "))]);
        assert!(r.contains("note required"), "{r}");
    }
}

#[test]
fn start_guard_honours_in_changeset_order() {
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "b", &["mara"], json!({})),
            add(2, "a", &["mara"], json!({"blocked_by": ids(&[1])})),
        ],
    );
    applied(&mut b, vec![st(1, "wip", None)]);
    let r = void(&mut b, vec![st(2, "wip", None)]);
    assert_eq!(
        r,
        format!(
            "{} cannot start: blocked by {} (wip)",
            short_id(&id(2)),
            short_id(&id(1))
        )
    );
    // A start, then B succeed: A was still blocked when it started
    void(&mut b, vec![st(2, "wip", None), st(1, "success", None)]);
    // B succeed, then A start: legal
    applied(&mut b, vec![st(1, "success", None), st(2, "wip", None)]);
    assert_eq!(state_of(&b, 2), TaskState::Wip);
    // a cancelled prerequisite counts as met; a failed one does not
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "x", &["mara"], json!({})),
            add(2, "y", &["mara"], json!({})),
            add(3, "z", &["mara"], json!({"blocked_by": ids(&[1, 2])})),
        ],
    );
    applied(
        &mut b,
        vec![
            st(1, "cancelled", Some("n")),
            st(2, "wip", None),
            st(2, "fail", Some("n")),
        ],
    );
    void(&mut b, vec![st(3, "wip", None)]);
    applied(
        &mut b,
        vec![
            st(2, "wip", None),
            st(2, "success", None),
            st(3, "wip", None),
        ],
    );
}

#[test]
fn an_appointment_closes_from_todo_a_floating_task_is_refused_the_same() {
    let timed = json!({"when": {"start": "2026-10-12", "end": "2026-10-12"}});
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "meeting", &["mara", "bot"], timed.clone()),
            add(2, "floating", &["mara"], json!({})),
            add(3, "prep", &["mara"], json!({})),
            add(4, "review", &["mara"], {
                let mut t = timed.clone();
                t["blocked_by"] = ids(&[3]);
                t
            }),
        ],
    );
    applied(&mut b, vec![st(1, "success", None)]);
    let r = void(&mut b, vec![st(2, "success", None)]);
    assert!(r.contains("not started"), "{r}");
    void(&mut b, vec![st(2, "fail", Some("n"))]);
    // the appointment's own prerequisites guard the close too
    let r = void(&mut b, vec![st(4, "success", None)]);
    assert!(r.contains("blocked by"), "{r}");
    applied(
        &mut b,
        vec![
            st(3, "cancelled", Some("dropped")),
            st(4, "fail", Some("nobody came")),
        ],
    );
    // start stays allowed for it
    let mut b = task_in(TaskState::Todo, true);
    applied(&mut b, vec![st(1, "wip", None), st(1, "success", None)]);
}

#[test]
fn a_series_only_cancels_and_reopens() {
    let series = json!({"when": {"start": "2026-10-05T09:00", "end": "2026-10-05T10:00"}, "repeat": {"freq": "weekly"}});
    let mut b = BoardState::default();
    applied(&mut b, vec![add(1, "sync", &["mara"], series)]);
    for to in ["wip", "success", "fail"] {
        let r = void(&mut b, vec![st(1, to, Some("n"))]);
        assert!(r.contains("a series only cancels or reopens"), "{r}");
    }
    applied(&mut b, vec![st(1, "cancelled", Some("over"))]);
    applied(&mut b, vec![st(1, "todo", None)]);
    // turning a wip task into a series voids
    applied(
        &mut b,
        vec![add(
            2,
            "t",
            &["mara"],
            json!({"when": {"start": "2026-10-05", "end": "2026-10-05"}}),
        )],
    );
    applied(&mut b, vec![st(2, "wip", None)]);
    void(&mut b, vec![set(2, json!({"repeat": {"freq": "daily"}}))]);
}

#[test]
fn links_to_or_from_a_series_void() {
    let series =
        json!({"when": {"start": "2026-10-05", "end": "2026-10-05"}, "repeat": {"freq": "daily"}});
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "sync", &["mara"], series),
            add(2, "work", &["mara"], json!({})),
        ],
    );
    let r = void(&mut b, vec![set(2, json!({"blocked_by": ids(&[1])}))]);
    assert!(r.contains("series cannot be linked"), "{r}");
    void(&mut b, vec![set(1, json!({"blocked_by": ids(&[2])}))]);
    // a linked once-timed task cannot become a series
    applied(
        &mut b,
        vec![
            add(
                3,
                "once",
                &["mara"],
                json!({"when": {"start": "2026-10-05", "end": "2026-10-05"}}),
            ),
            set(2, json!({"blocked_by": ids(&[3])})),
        ],
    );
    let r = void(&mut b, vec![set(3, json!({"repeat": {"freq": "daily"}}))]);
    assert!(r.contains("series cannot be linked"), "{r}");
}

#[test]
fn cycles_void() {
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "a", &["mara"], json!({})),
            add(2, "b", &["mara"], json!({"blocked_by": ids(&[1])})),
            add(3, "c", &["mara"], json!({"blocked_by": ids(&[2])})),
        ],
    );
    let r = void(&mut b, vec![set(1, json!({"blocked_by": ids(&[3])}))]);
    assert!(r.contains("cycle"), "{r}");
    // a cycle formed inside one changeset of adds
    let r = void(
        &mut b,
        vec![
            add(4, "d", &["mara"], json!({"blocked_by": ids(&[5])})),
            add(5, "e", &["mara"], json!({"blocked_by": ids(&[4])})),
        ],
    );
    assert!(r.contains("cycle"), "{r}");
    // a forward link inside one changeset is fine
    applied(
        &mut b,
        vec![
            add(4, "d", &["mara"], json!({"blocked_by": ids(&[5])})),
            add(5, "e", &["mara"], json!({})),
        ],
    );
}

#[test]
fn unknown_tasks_and_unknown_seats_void() {
    let mut b = BoardState::default();
    let r = void(&mut b, vec![st(9, "wip", None)]);
    assert!(r.contains("unknown task"), "{r}");
    let r = void(&mut b, vec![set(9, json!({"title": "x"}))]);
    assert!(r.contains("unknown task"), "{r}");
    let r = void(
        &mut b,
        vec![add(1, "a", &["mara"], json!({"blocked_by": ids(&[9])}))],
    );
    assert!(r.contains("blocked by unknown"), "{r}");
    let r = void(&mut b, vec![add(1, "a", &["eve"], json!({}))]);
    assert!(r.contains("eve is not a seat"), "{r}");
    let mut a = add(1, "a", &["mara"], json!({}));
    a["creator"] = json!("eve");
    let r = void(&mut b, vec![a]);
    assert!(r.contains("creator eve"), "{r}");
}

#[test]
fn resulting_field_rules_void() {
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![add(1, "a", &["mara"], json!({"due": "2026-10-20"}))],
    );
    let r = void(&mut b, vec![set(1, json!({"after": "2026-10-21"}))]);
    assert!(r.contains("after is later than due"), "{r}");
    let r = void(&mut b, vec![set(1, json!({"repeat": {"freq": "daily"}}))]);
    assert!(r.contains("repeat needs when"), "{r}");
    let r = void(&mut b, vec![set(1, json!({"skip": ["2026-10-20"]}))]);
    assert!(r.contains("need a series"), "{r}");
    applied(
        &mut b,
        vec![add(
            2,
            "sync",
            &["mara"],
            json!({
        "when": {"start": "2026-10-05T09:00", "end": "2026-10-05T10:00"},
        "repeat": {"freq": "weekly"}}),
        )],
    );
    applied(
        &mut b,
        vec![set(
            2,
            json!({"skip": ["2026-10-12"],
        "moved": {"2026-10-19": {"start": "2026-10-20T09:00", "end": "2026-10-20T10:00"}}}),
        )],
    );
    let r = void(&mut b, vec![set(2, json!({"skip": ["2026-10-13"]}))]);
    assert!(r.contains("2026-10-13 is no occurrence"), "{r}");
    // moving the series start strands the old keys
    let r = void(
        &mut b,
        vec![set(
            2,
            json!({"when": {"start": "2026-10-06T09:00", "end": "2026-10-06T10:00"}}),
        )],
    );
    assert!(r.contains("is no occurrence"), "{r}");
    // clearing when strands the repeat
    void(&mut b, vec![set(2, json!({"when": null}))]);
}

#[test]
fn state_sets_note_and_evidence_and_set_replaces_fields() {
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![add(
            1,
            "a",
            &["mara"],
            json!({"acceptance": ["fast", "small"], "type": "bug", "size": "S"}),
        )],
    );
    applied(&mut b, vec![st(1, "wip", Some("go"))]);
    applied(
        &mut b,
        vec![json!({"act": "state", "id": id(1), "to": "success", "evidence": ["1.2 s", ""]})],
    );
    let t = &b.tasks[&id(1)];
    assert_eq!(t.evidence, ["1.2 s", ""]);
    assert_eq!(t.note, None);
    applied(&mut b, vec![st(1, "todo", Some("reopened"))]);
    assert!(b.tasks[&id(1)].evidence.is_empty());
    applied(
        &mut b,
        vec![set(
            1,
            json!({"type": null, "size": null, "acceptance": null, "assignees": ["bot", "walter"]}),
        )],
    );
    let t = &b.tasks[&id(1)];
    assert_eq!(
        (t.kind.as_deref(), t.size, t.acceptance.len()),
        (None, None, 0)
    );
    assert_eq!(t.assignees, ["bot", "walter"]);
    assert_eq!(t.creator, "mara");
}

#[test]
fn precheck_names_the_first_reason_and_leaves_the_board() {
    let mut b = BoardState::default();
    applied(
        &mut b,
        vec![
            add(1, "b", &["mara"], json!({})),
            add(2, "a", &["mara"], json!({"blocked_by": ids(&[1])})),
        ],
    );
    let before = b.clone();
    let p = cs(vec![st(2, "wip", None), st(1, "wip", None)]);
    let r = kanban_precheck(&b, &p, &seats()).expect_err("blocked");
    assert_eq!(
        r.0,
        format!(
            "{} cannot start: blocked by {} (todo)",
            short_id(&id(2)),
            short_id(&id(1))
        )
    );
    assert_eq!(b, before);
    assert!(kanban_precheck(&b, &cs(vec![st(1, "wip", None)]), &seats()).is_ok());
    // the precheck verdict is the fold's
    let mut folded = b.clone();
    assert!(matches!(
        kanban_fold_one(&mut folded, &p, &seats()),
        FoldStep::Void(_)
    ));
}

fn refused(v: &Value) -> String {
    validate_kanban_payload(v).expect_err("refused")
}

#[test]
fn shape_check_refuses() {
    let ok = add(1, "a", &["mara"], json!({}));
    assert!(validate_kanban_payload(&cs(vec![ok.clone()])).is_ok());
    assert!(validate_kanban_wire(&cs(vec![ok.clone()])).is_ok());
    assert!(refused(&json!([])).contains("not an object"));
    assert!(refused(&json!({"op": "wiki_patch"})).contains("kanban_ops"));
    assert!(refused(&cs(vec![])).contains("at least one act"));
    let mut v = cs(vec![ok.clone()]);
    v["extra"] = json!(1);
    assert!(refused(&v).contains("unknown field"));
    v = cs(vec![ok.clone()]);
    v["summary"] = json!("two\nlines");
    assert!(refused(&v).contains("summary"));
    v = cs(vec![ok.clone()]);
    v.as_object_mut().expect("obj").remove("base_rev");
    assert!(refused(&v).contains("base_rev"));
    let bad: Vec<(Value, &str)> = vec![
        (json!({"act": "move", "id": id(1)}), "unknown act"),
        (
            add(1, "a", &["mara"], json!({"colour": "red"})),
            "unknown field",
        ),
        (add(1, "a", &[], json!({})), "assignees"),
        (add(1, "a", &["mara", "mara"], json!({})), "twice"),
        (add(1, "", &["mara"], json!({})), "title"),
        (add(1, "a\nb", &["mara"], json!({})), "title"),
        (add(1, "a", &["mara"], json!({"size": "XXXL"})), "size"),
        (add(1, "a", &["mara"], json!({"due": "2026-8-1"})), "due"),
        (
            add(
                1,
                "a",
                &["mara"],
                json!({"after": "2026-10-21", "due": "2026-10-20"}),
            ),
            "after is later than due",
        ),
        (
            add(
                1,
                "a",
                &["mara"],
                json!({"when": {"start": "2026-10-02", "end": "2026-10-01"}}),
            ),
            "when",
        ),
        (
            add(
                1,
                "a",
                &["mara"],
                json!({"when": {"start": "2026-10-01", "end": "2026-10-01T10:00"}}),
            ),
            "when",
        ),
        (
            add(1, "a", &["mara"], json!({"repeat": {"freq": "daily"}})),
            "repeat needs when",
        ),
        (
            add(1, "a", &["mara"], json!({"acceptance": ["ok", ""]})),
            "acceptance",
        ),
        (
            add(1, "a", &["mara"], json!({"out_of_scope": ["a\nb"]})),
            "out_of_scope",
        ),
        (
            add(1, "a", &["mara"], json!({"blocked_by": ids(&[1])})),
            "itself",
        ),
        (
            add(1, "a", &["mara"], json!({"blocked_by": ids(&[2, 2])})),
            "twice",
        ),
        (add(1, "a", &["mara"], json!({"due": null})), "null"),
        (add(1, "a", &["mara"], json!({"type": " "})), "type"),
        (
            json!({"act": "add", "id": "ABC", "title": "a", "assignees": ["mara"]}),
            "not a task id",
        ),
        (set(1, json!({})), "names nothing"),
        (set(1, json!({"state": "wip"})), "not settable"),
        (set(1, json!({"creator": "bot"})), "not settable"),
        (set(1, json!({"note": "x"})), "not settable"),
        (set(1, json!({"evidence": []})), "not settable"),
        (set(1, json!({"title": null})), "cannot be cleared"),
        (set(1, json!({"blocked_by": ids(&[1])})), "itself"),
        (
            json!({"act": "state", "id": id(1), "to": "done"}),
            "unknown state",
        ),
        (
            json!({"act": "state", "id": id(1), "to": "fail", "note": "n", "evidence": ["x"]}),
            "evidence only with success",
        ),
        (
            json!({"act": "state", "id": "@x", "to": "wip"}),
            "unresolved",
        ),
    ];
    for (act, want) in bad {
        let r = refused(&cs(vec![act.clone()]));
        assert!(r.contains(want), "{act}: {r}");
    }
}

#[test]
fn refs_resolve_at_propose_and_are_refused_on_the_wire() {
    let p = cs(vec![
        json!({"act": "add", "ref": "api", "title": "api", "assignees": ["mara"]}),
        json!({"act": "add", "ref": "client", "title": "client", "assignees": ["walter"], "blocked_by": ["@api"]}),
        json!({"act": "state", "id": "@api", "to": "wip"}),
    ]);
    assert!(validate_kanban_payload(&p).is_ok());
    assert!(validate_kanban_wire(&p).is_err());
    let dup = cs(vec![
        json!({"act": "add", "ref": "a", "title": "x", "assignees": ["mara"]}),
        json!({"act": "add", "ref": "a", "title": "y", "assignees": ["mara"]}),
    ]);
    assert!(refused(&dup).contains("twice"));
    let selfref = cs(vec![
        json!({"act": "add", "ref": "a", "title": "x", "assignees": ["mara"], "blocked_by": ["@a"]}),
    ]);
    assert!(refused(&selfref).contains("itself"));
    assert!(refused(&cs(vec![
        json!({"act": "add", "ref": "A B", "title": "x", "assignees": ["mara"]})
    ]))
    .contains("ref"));
    // the wire wants every add canonical
    let mut no_creator = add(1, "a", &["mara"], json!({}));
    no_creator.as_object_mut().expect("obj").remove("creator");
    assert!(validate_kanban_payload(&cs(vec![no_creator.clone()])).is_ok());
    let mut odd_creator = no_creator.clone();
    odd_creator["creator"] = json!(7);
    assert!(validate_kanban_payload(&cs(vec![odd_creator.clone()])).is_ok());
    assert!(validate_kanban_wire(&cs(vec![odd_creator])).is_err());
    assert!(validate_kanban_wire(&cs(vec![no_creator]))
        .expect_err("wire")
        .contains("creator"));
    let mut no_id = add(1, "a", &["mara"], json!({}));
    no_id.as_object_mut().expect("obj").remove("id");
    assert!(validate_kanban_wire(&cs(vec![no_id]))
        .expect_err("wire")
        .contains("id missing"));
    let with_ref = add(1, "a", &["mara"], json!({"ref": "a"}));
    assert!(validate_kanban_wire(&cs(vec![with_ref]))
        .expect_err("wire")
        .contains("ref"));
    let at = add(1, "a", &["mara"], json!({"blocked_by": ["@b"]}));
    assert!(validate_kanban_wire(&cs(vec![at])).is_err());
    let untrimmed = add(1, "a", &["mara"], json!({"type": "bug "}));
    assert!(validate_kanban_payload(&cs(vec![untrimmed.clone()])).is_ok());
    assert!(validate_kanban_wire(&cs(vec![untrimmed]))
        .expect_err("wire")
        .contains("trimmed"));
}

#[test]
fn dates_that_do_not_round_trip_are_refused() {
    for (k, v) in [
        ("due", json!("2026-8-1")),
        ("after", json!("2026-08-1")),
        ("due", json!("2026-02-30")),
        (
            "when",
            json!({"start": "2026-10-01T9:00", "end": "2026-10-01T10:00"}),
        ),
        (
            "when",
            json!({"start": "2026-10-01T09:00:00", "end": "2026-10-01T10:00:00"}),
        ),
        ("skip", json!(["2026-10-1"])),
        (
            "moved",
            json!({"2026-10-1": {"start": "2026-10-02", "end": "2026-10-02"}}),
        ),
    ] {
        let mut extra = json!({"when": {"start": "2026-10-01", "end": "2026-10-01"}, "repeat": {"freq": "daily"}});
        extra[k] = v;
        assert!(
            validate_kanban_payload(&cs(vec![add(1, "a", &["mara"], extra)])).is_err(),
            "{k}"
        );
    }
    assert!(validate_kanban_payload(&cs(vec![set(1, json!({"due": "2026-8-1"}))])).is_err());
}

/// The fixture board whose canonical bytes are pinned.
pub(crate) fn fixture_board() -> BoardState {
    fold(&[
        cs(vec![
            add(
                1,
                "API schema",
                &["mara"],
                json!({"type": "feature", "size": "M",
                "description": "the *what*", "acceptance": ["documented"], "out_of_scope": ["v2"]}),
            ),
            add(
                2,
                "Client",
                &["walter", "bot"],
                json!({"blocked_by": ids(&[1]), "due": "2026-10-23", "after": "2026-10-13"}),
            ),
            add(
                3,
                "Weekly sync",
                &["mara", "walter"],
                json!({
                "when": {"start": "2026-10-05T09:00", "end": "2026-10-05T09:30"},
                "repeat": {"freq": "weekly", "byday": ["mo", "th"], "until": "2027-06-30"},
                "skip": ["2026-10-08"],
                "moved": {"2026-10-12": {"start": "2026-10-13T09:00", "end": "2026-10-13T09:30"}}}),
            ),
        ]),
        cs(vec![st(1, "wip", Some("started"))]),
        cs(vec![
            json!({"act": "state", "id": id(1), "to": "success", "evidence": ["in the wiki"]}),
        ]),
    ])
}

#[test]
fn the_fixture_board_is_byte_pinned() {
    let got = serde_json::to_string(&fixture_board().to_json()).expect("json");
    let want = concat!(
        r#"{"rev":3,"tasks":{"#,
        r#""00000000000000000000000000000001":{"acceptance":["documented"],"assignees":["mara"],"creator":"mara","description":"the *what*","evidence":["in the wiki"],"out_of_scope":["v2"],"size":"M","state":"success","title":"API schema","touched_rev":3,"type":"feature"},"#,
        r#""00000000000000000000000000000002":{"after":"2026-10-13","assignees":["walter","bot"],"blocked_by":["00000000000000000000000000000001"],"creator":"mara","due":"2026-10-23","state":"todo","title":"Client","touched_rev":1},"#,
        r#""00000000000000000000000000000003":{"assignees":["mara","walter"],"creator":"mara","moved":{"2026-10-12":{"end":"2026-10-13T09:30","start":"2026-10-13T09:00"}},"repeat":{"byday":["mo","th"],"freq":"weekly","interval":1,"until":"2027-06-30"},"skip":["2026-10-08"],"state":"todo","title":"Weekly sync","touched_rev":1,"when":{"end":"2026-10-05T09:30","start":"2026-10-05T09:00"}}"#,
        r#"}}"#
    );
    assert_eq!(got, want);
}
