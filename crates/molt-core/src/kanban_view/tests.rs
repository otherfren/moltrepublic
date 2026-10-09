use super::*;
use crate::kanban_calendar::parse_date;
use crate::kanban_dates::derive;
use crate::kanban_fold::tests::{add, cs, fold, id, ids, set, st};
use crate::kanban_fold::{short_id, BoardState};
use crate::kanban_review::board_view;
use serde_json::json;

fn d(s: &str) -> NaiveDate {
    parse_date(s).expect("fixture date")
}

/// Mon 2026-10-12, 12:00 UTC.
fn now() -> NaiveDateTime {
    d("2026-10-12").and_hms_opt(12, 0, 0).expect("noon")
}

/// 1 api (mara, wip) <- 2 client (walter, blocked); 3 docs (bot, with
/// text); 4 meet (mara+walter, today 09:00); 5 sync (mara, weekly mondays
/// 14:00); 6 old (mara, success at rev 2), 7 gone (mara, success at rev 3);
/// then `pad` changesets touching only 3.
fn board(pad: usize) -> BoardState {
    let mut sets = vec![
        cs(vec![
            add(1, "api", &["mara"], json!({"size": "M", "type": "Feature"})),
            add(2, "client", &["walter"], json!({"blocked_by": ids(&[1]), "type": "feature"})),
            add(3, "docs", &["bot"], json!({"description": "why", "acceptance": ["x"], "type": "Bug"})),
            add(4, "meet", &["mara", "walter"], json!({"when": {"start": "2026-10-12T09:00", "end": "2026-10-12T10:00"}})),
            add(5, "sync", &["mara"], json!({"when": {"start": "2026-10-05T14:00", "end": "2026-10-05T15:00"}, "repeat": {"freq": "weekly"}})),
            add(6, "old", &["mara"], json!({})),
            add(7, "gone", &["mara"], json!({})),
            st(1, "wip", None),
            st(6, "wip", None),
        ]),
        cs(vec![st(6, "success", None)]),
        cs(vec![st(7, "cancelled", Some("dropped"))]),
    ];
    for _ in 0..pad {
        sets.push(cs(vec![set(3, json!({"size": "S"}))]));
    }
    fold(&sets)
}

fn snapshot(board: &BoardState, pending: Value) -> SurfaceSnapshot {
    let dv = derive(board, now().date());
    serde_json::from_value(json!({
        "surface": "quests",
        "gated": true,
        "applied": [],
        "pending": pending,
        "board": board_view(board, &dv, now(), "mara"),
    }))
    .expect("a snapshot")
}

fn view(snap: &SurfaceSnapshot, q: &ViewQuery) -> Value {
    quests_view(snap, q).expect("a view")
}

fn task_ids(v: &Value) -> Vec<String> {
    v["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|t| t["id"].as_str().expect("id").to_string())
        .collect()
}

fn filtered(snap: &SurfaceSnapshot, names: &[&str]) -> Vec<String> {
    let filter = names
        .iter()
        .map(|n| ViewFilter::parse(n).expect("a filter"))
        .collect();
    task_ids(&view(snap, &ViewQuery { filter, ..ViewQuery::default() }))
}

#[test]
fn the_default_view_lists_open_work_by_priority_then_recently_closed() {
    let snap = snapshot(&board(0), json!([]));
    let v = view(&snap, &ViewQuery::default());
    assert_eq!(v["rev"], json!(3));
    assert_eq!(v["seat"], json!("mara"));
    let got = task_ids(&v);
    assert_eq!(got[0], id(1), "wip first");
    assert_eq!(&got[got.len() - 2..], [id(7), id(6)], "closed last, newest first");
    let api = &v["tasks"][0];
    assert_eq!(api["shown"], json!("wip"));
    assert!(api.get("description").is_none());
    let docs = v["tasks"].as_array().expect("tasks").iter().find(|t| t["id"] == json!(id(3))).expect("docs");
    assert!(docs.get("description").is_none() && docs.get("acceptance").is_none(), "texts ride task, not the list");
    assert!(v["next"]["mara"].is_array(), "next per seat without a seat");
}

#[test]
fn closed_work_leaves_the_default_list_after_ten_changesets_but_not_the_closed_filter() {
    let snap = snapshot(&board(usize::try_from(RECENT_CLOSED_REVS - 1).expect("small")), json!([]));
    let got = task_ids(&view(&snap, &ViewQuery::default()));
    assert!(!got.contains(&id(6)), "rev 2 is past the window");
    assert!(got.contains(&id(7)), "rev 3 is the last one in it");
    assert_eq!(filtered(&snap, &["closed"]), [id(7), id(6)]);
    assert_eq!(filtered(&snap, &["success"]), [id(6)]);
}

#[test]
fn the_seat_relative_filters_follow_the_seat() {
    let snap = snapshot(&board(0), json!([]));
    assert_eq!(filtered(&snap, &["mine", "wip"]), [id(1)]);
    assert!(filtered(&snap, &["mine"]).contains(&id(4)));
    assert_eq!(filtered(&snap, &["created_by_me", "blocked"]), [id(2)]);
    assert_eq!(filtered(&snap, &["starting_now"]), [id(4)], "09:00 has passed, 14:00 has not");
    let walter = ViewQuery {
        seat: Some("walter".into()),
        filter: vec![ViewFilter::Mine],
        ..ViewQuery::default()
    };
    let v = view(&snap, &walter);
    assert_eq!(task_ids(&v), [id(2), id(4)]);
    assert!(v["next"].is_array(), "one seat, one list");
}

#[test]
fn groups_combine_or_within_and_across() {
    let snap = snapshot(&board(0), json!([]));
    assert_eq!(filtered(&snap, &["type:feature"]), [id(1), id(2)], "one type, any spelling");
    assert_eq!(filtered(&snap, &["type:bug", "type:FEATURE", "blocked"]), [id(2)]);
    assert_eq!(filtered(&snap, &["size:M", "size:XL"]), [id(1)]);
    assert_eq!(filtered(&snap, &["timed", "mine"]).len(), 2);
    assert!(!filtered(&snap, &["floating"]).contains(&id(4)));
}

#[test]
fn an_unknown_filter_names_the_vocabulary() {
    let err = ViewFilter::parse("urgent").expect_err("refused");
    assert!(err.contains("to_act_on") && err.contains("type:"), "{err}");
    assert!(ViewFilter::parse("size:XXXL").is_err());
    assert!(ViewFilter::parse("type:").is_err());
}

#[test]
fn needs_my_vote_keeps_tasks_a_pending_changeset_touches() {
    let pending = json!([{
        "id": 9, "surface": "quests", "approvals": 1, "threshold": 2, "state": "proposed",
        "payload": cs(vec![st(3, "wip", None)]),
        "votes": [{"member": "mara", "vote": "open"}, {"member": "walter", "vote": "approved"}],
    }]);
    let snap = snapshot(&board(0), pending);
    assert_eq!(filtered(&snap, &["needs_my_vote"]), [id(3)]);
    let walter = ViewQuery {
        seat: Some("walter".into()),
        filter: vec![ViewFilter::NeedsMyVote],
        ..ViewQuery::default()
    };
    assert!(task_ids(&view(&snap, &walter)).is_empty(), "walter voted");
}

#[test]
fn one_task_reads_with_its_text_and_prerequisites_to_any_depth() {
    let b = fold(&[cs(vec![
        add(1, "a", &["mara"], json!({})),
        add(2, "b", &["mara"], json!({"blocked_by": ids(&[1])})),
        add(3, "c", &["mara"], json!({"blocked_by": ids(&[2, 1]), "acceptance": ["done"]})),
        st(1, "wip", None),
    ])]);
    let snap = snapshot(&b, json!([]));
    let q = ViewQuery { task: Some(format!("#{}", id(3))), ..ViewQuery::default() };
    let v = view(&snap, &q);
    assert!(v.get("tasks").is_none(), "one task, not the board");
    let t = &v["task"];
    assert_eq!(t["id"], json!(id(3)));
    assert_eq!(t["acceptance"], json!(["done"]));
    let pre: Vec<(&str, u64)> = t["prerequisites"]
        .as_array()
        .expect("prerequisites")
        .iter()
        .map(|p| (p["id"].as_str().expect("id"), p["depth"].as_u64().expect("depth")))
        .collect();
    assert_eq!(pre, [(id(2).as_str(), 1), (id(1).as_str(), 1)]);
    assert_eq!(t["progress"], json!({"succeeded": 0, "total": 2}));
    let a = view(&snap, &ViewQuery { task: Some(id(1)), ..ViewQuery::default() });
    assert_eq!(a["task"]["prerequisite_for"], json!([id(2), id(3)]));
    let deep = view(&snap, &ViewQuery { task: Some(id(2)), ..ViewQuery::default() });
    assert_eq!(deep["task"]["prerequisites"][0]["id"], json!(id(1)));
}

#[test]
fn a_task_is_named_by_id_or_unique_prefix() {
    let snap = snapshot(&board(0), json!([]));
    let unknown = ViewQuery { task: Some("#ffffffff".into()), ..ViewQuery::default() };
    assert!(quests_view(&snap, &unknown).expect_err("unknown").contains("unknown task"));
    let ambiguous = ViewQuery { task: Some("0000".into()), ..ViewQuery::default() };
    assert!(quests_view(&snap, &ambiguous).expect_err("ambiguous").contains("ambiguous"));
}

#[test]
fn the_calendar_expands_timed_tasks_in_the_window() {
    let snap = snapshot(&board(0), json!([]));
    let q = ViewQuery {
        from: Some(d("2026-10-12")),
        to: Some(d("2026-10-19")),
        ..ViewQuery::default()
    };
    let v = view(&snap, &q);
    let cal: Vec<(String, String)> = v["calendar"]
        .as_array()
        .expect("calendar")
        .iter()
        .map(|o| (o["task"].as_str().expect("task").to_string(), o["start"].as_str().expect("start").to_string()))
        .collect();
    assert_eq!(
        cal,
        [
            (id(4), "2026-10-12T09:00".to_string()),
            (id(5), "2026-10-12T14:00".to_string()),
            (id(5), "2026-10-19T14:00".to_string()),
        ]
    );
    assert_eq!(v["calendar"][1]["occurrence"], json!("2026-10-12"));
    assert!(v["calendar"][0].get("occurrence").is_none(), "a once-timed task has one window");
}

#[test]
fn the_calendar_window_is_whole_and_bounded() {
    let half = ViewQuery { from: Some(d("2026-10-12")), ..ViewQuery::default() };
    assert!(half.window().is_err());
    let back = ViewQuery { from: Some(d("2026-10-12")), to: Some(d("2026-10-11")), ..ViewQuery::default() };
    assert!(back.window().is_err());
    let long = ViewQuery { from: Some(d("2026-01-01")), to: Some(d("2027-01-02")), ..ViewQuery::default() };
    assert!(long.window().is_err());
    let year = ViewQuery { from: Some(d("2026-01-01")), to: Some(d("2027-01-01")), ..ViewQuery::default() };
    assert!(year.window().is_ok());
}

#[test]
fn the_review_read_splits_the_advisories() {
    let pending = json!([{
        "id": 9, "surface": "quests", "approvals": 1, "threshold": 2, "state": "proposed",
        "by": "walter",
        "payload": {"op": "kanban_ops", "summary": "move docs", "base_rev": 1, "ops": [st(3, "wip", None)]},
        "rendered": ["#00000003 state: todo → wip"],
        "advisories": [
            "would void now: x",
            "changed since proposed: #00000003 size",
            "#00000003: needed by none → 2026-10-20",
        ],
    }]);
    let snap = snapshot(&board(0), pending);
    let v = view(&snap, &ViewQuery { proposal: Some(9), ..ViewQuery::default() });
    let p = &v["proposal"];
    assert_eq!(p["summary"], json!("move docs"));
    assert_eq!(p["state"], json!("proposed"));
    assert_eq!(p["acts"], json!(["#00000003 state: todo → wip"]));
    assert_eq!(p["would_void"], json!(["x"]));
    assert_eq!(p["changed_since"], json!(["#00000003 size"]));
    assert_eq!(p["impact"], json!(["#00000003: needed by none → 2026-10-20"]));
    assert_eq!(p["ops"][0]["to"], json!("wip"), "the signed acts ride along");
    let missing = ViewQuery { proposal: Some(8), ..ViewQuery::default() };
    assert!(quests_view(&snap, &missing).is_err());
}

#[test]
fn a_decided_proposal_renders_its_acts_without_the_before() {
    let accepted = json!([{
        "id": 5, "surface": "quests", "approvals": 2, "threshold": 2, "state": "applied",
        "payload": cs(vec![st(1, "wip", None)]),
    }]);
    let mut snap = snapshot(&board(0), json!([]));
    snap.accepted = serde_json::from_value(accepted).expect("views");
    let v = view(&snap, &ViewQuery { proposal: Some(5), ..ViewQuery::default() });
    assert_eq!(v["proposal"]["acts"], json!([format!("{} state: wip", short_id(&id(1)))]));
    assert_eq!(v["proposal"]["state"], json!("applied"));
}
