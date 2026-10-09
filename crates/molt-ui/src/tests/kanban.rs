// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban projections (`kanban_workflows.md` §8) over a board folded
//! and derived by molt-core, exactly as the engine serves it.

use std::collections::BTreeSet;

use molt_core::kanban_dates::derive;
use molt_core::kanban_fold::{kanban_fold, short_id};
use molt_core::kanban_review::board_view;
use molt_core::kanban_view::type_slot;
use molt_core::kanban_wake::WakeAction;
use molt_core::SurfaceSnapshot;
use serde_json::{json, Value};

use crate::i18n::Lexicon;
use crate::kanban::*;

fn id(n: u32) -> String {
    format!("{n:032x}")
}

fn add(n: u32, title: &str, who: &[&str], extra: Value) -> Value {
    let mut v = json!({"act": "add", "id": id(n), "creator": "mara", "title": title, "assignees": who});
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    v
}

fn st(n: u32, to: &str, note: Option<&str>) -> Value {
    let mut v = json!({"act": "state", "id": id(n), "to": to});
    if let Some(note) = note {
        v["note"] = Value::from(note);
    }
    v
}

fn cs(ops: Vec<Value>) -> Value {
    json!({"op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": ops})
}

fn feed_with(applied: Vec<Value>, pending: Value, me: &str) -> KanbanFeed {
    let seats = ["mara", "walter", "bot"];
    let set: BTreeSet<String> = seats.iter().map(|s| (*s).to_string()).collect();
    let board = kanban_fold(&applied, &set);
    let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 12)
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .expect("noon");
    let derived = derive(&board, now.date());
    let view = board_view(&board, &derived, now, me, 10);
    let mut snap: SurfaceSnapshot = serde_json::from_value(json!({
        "surface": "quests", "gated": true, "applied": applied, "pending": pending,
    }))
    .expect("a snapshot");
    snap.board = Some(view);
    KanbanFeed { snap, actions: Vec::new(), seats: seats.iter().map(|s| (*s).to_string()).collect(), me: me.to_string() }
}

/// 1 api (mara, wip); 2 client blocked by 1; 3 docs open (Bug); 4 meet
/// timed once; 5 broken (fail); 6 waits on 5 (stuck); 7 done long ago.
fn feed() -> KanbanFeed {
    let mut applied = vec![cs(vec![
        add(1, "api", &["mara"], json!({"type": "Bug", "size": "M"})),
        add(2, "client", &["walter"], json!({"blocked_by": [id(1)], "type": "bug"})),
        add(3, "docs", &["bot"], json!({"type": "bug", "acceptance": ["a", "b"]})),
        add(4, "meet", &["mara", "walter"], json!({"when": {"start": "2026-10-13T09:00", "end": "2026-10-13T10:00"}})),
        add(5, "broken", &["walter"], json!({})),
        add(6, "waits", &["walter"], json!({"blocked_by": [id(5)]})),
        add(7, "old", &["mara"], json!({})),
        st(1, "wip", None),
        st(5, "wip", None),
        st(7, "wip", None),
    ])];
    applied.push(cs(vec![st(5, "fail", Some("no way"))]));
    applied.push(cs(vec![st(7, "success", None)]));
    for _ in 0..12 {
        applied.push(cs(vec![json!({"act": "set", "id": id(3), "fields": {"size": "S"}})]));
    }
    feed_with(applied, json!([]), "mara")
}

fn ids(cards: &[crate::KbCard]) -> Vec<String> {
    cards.iter().map(|c| c.id.to_string()).collect()
}

#[test]
fn the_board_splits_by_state_and_sorts_to_do_by_sub_state() {
    let l = Lexicon::en();
    let b = board(&l, &feed(), &BTreeSet::new(), &Basket::default());
    assert_eq!(ids(&b.cols[0]), [id(2), id(6), id(4), id(3)], "blocked, stuck, scheduled, open");
    assert_eq!(ids(&b.cols[1]), [id(1)]);
    // the old success left the default list after ten changesets
    assert_eq!(ids(&b.cols[2]), Vec::<String>::new());
    let stuck = &b.cols[0][1];
    assert_eq!(stuck.badge, "stuck");
    assert!(stuck.bad);
    assert_eq!(b.cols[0][0].badge, "blocked");
}

/// §2.4: one spelling and one colour per folded type.
#[test]
fn a_card_carries_its_type_chip_and_size() {
    let l = Lexicon::en();
    let b = board(&l, &feed(), &BTreeSet::new(), &Basket::default());
    let api = &b.cols[1][0];
    assert_eq!(api.type_label, "bug", "the spelling most tasks use");
    assert_eq!(api.type_slot, i32::from(type_slot("bug").expect("a slot")));
    assert_eq!(api.size, "M");
    assert_eq!(b.legend.len(), 1);
    assert_eq!(b.legend[0].label, "bug");
}

/// The closed filter replaces the archive: every closed task, with its note.
#[test]
fn the_closed_filter_is_the_archive() {
    let l = Lexicon::en();
    let on: BTreeSet<String> = ["closed".to_string()].into();
    let b = board(&l, &feed(), &on, &Basket::default());
    assert!(b.cols[0].is_empty() && b.cols[1].is_empty());
    assert_eq!(ids(&b.cols[2]), [id(7), id(5)], "newest closed first");
    let broken = b.cols[2].iter().find(|c| c.id.as_str() == id(5)).expect("the failed task");
    assert_eq!(broken.note, "no way");
}

#[test]
fn a_pending_changeset_badges_the_cards_it_touches() {
    let l = Lexicon::en();
    let pending = json!([{
        "id": 9, "surface": "quests", "approvals": 1, "threshold": 2, "state": "proposed",
        "payload": cs(vec![st(1, "success", None)]),
    }]);
    let f = feed_with(feed().snap.applied, pending, "mara");
    let b = board(&l, &f, &BTreeSet::new(), &Basket::default());
    assert_eq!(b.cols[1][0].vote, "vote: → done (1/2)");
    assert_eq!(b.cols[1][0].vote_id, 9);
    assert_eq!(b.cols[0][0].vote, "");
}

/// §8: a drop stages; the card stays and a dashed shadow waits in the
/// target column.
#[test]
fn a_staged_move_leaves_the_card_and_adds_a_shadow() {
    let l = Lexicon::en();
    let f = feed();
    let mut basket = Basket::default();
    let to = drop_target(&f, &id(3), 1).expect("open work may start");
    basket.stage_state(&id(3), to, "", &[]);
    let b = board(&l, &f, &BTreeSet::new(), &basket);
    let docs = b.cols[0].iter().find(|c| c.id.as_str() == id(3)).expect("still in to do");
    assert!(!docs.shadow);
    assert_eq!(docs.staged, "in basket → in progress");
    assert!(b.cols[1][0].shadow);
    assert_eq!(b.cols[1][0].id.as_str(), id(3));
}

#[test]
fn only_legal_drops_stage() {
    let f = feed();
    assert_eq!(drop_target(&f, &id(3), 1), Some("wip"));
    assert_eq!(drop_target(&f, &id(3), 2), None, "floating work starts first");
    assert_eq!(drop_target(&f, &id(4), 2), Some("success"), "an appointment closes from to do");
    assert_eq!(drop_target(&f, &id(1), 2), Some("success"));
    assert_eq!(drop_target(&f, &id(1), 0), Some("todo"));
    assert_eq!(drop_target(&f, &id(5), 1), Some("wip"), "retry");
    assert_eq!(drop_target(&f, &id(7), 1), None, "reopen first");
    assert_eq!(drop_target(&f, &id(7), 0), Some("todo"));
    assert_eq!(drop_target(&f, &id(1), 1), None, "same column");
}

/// The basket shows what the voter will see: the acts, would-void, the
/// missing-content warnings - before anything is proposed.
#[test]
fn the_basket_review_is_the_voters_view() {
    let l = Lexicon::en();
    let f = feed();
    let mut basket = Basket::default();
    basket.stage_state(&id(2), "wip", "", &[]);
    let form = Form { title: "write".into(), assignees: vec!["mara".into()], ..Form::default() };
    basket.add(form_act(&form).expect("a valid add"));
    let r = review(&l, &f, &basket);
    assert!(r.acts.iter().any(|a| a == "new 1 add: write"), "{:?}", r.acts);
    assert!(r.acts.iter().any(|a| a.starts_with(&short_id(&id(2)))), "{:?}", r.acts);
    assert!(r.advisories.iter().any(|a| a.starts_with("would void now: ")), "{:?}", r.advisories);
    assert!(r.advisories.iter().any(|a| a.ends_with("no acceptance criteria")), "{:?}", r.advisories);
    assert_eq!(auto_summary(&r.acts).matches("(+").count(), 1);
}

#[test]
fn the_basket_round_trips_and_rescues() {
    let mut basket = Basket::default();
    assert_eq!(basket.to_draft(), "");
    basket.stage_state(&id(1), "wip", "", &[]);
    basket.stage_state(&id(1), "fail", "why", &[]);
    assert_eq!(basket.acts.len(), 1, "one move per card");
    assert_eq!(basket.acts[0]["note"], json!("why"));
    let back = Basket::from_draft(&basket.to_draft());
    assert_eq!(back, basket);
    assert_eq!(Basket::from_draft("not json"), Basket::default());
    let n = basket.rescue(&cs(vec![st(3, "wip", None), st(4, "wip", None)]));
    assert_eq!(n, 2);
    assert_eq!(basket.acts.len(), 3);
    assert_eq!(basket.summary, "s");
    let p = basket.payload(7, "auto");
    assert_eq!(p["base_rev"], json!(7));
    assert_eq!(p["summary"], json!("s"));
}

#[test]
fn success_carries_evidence_per_criterion() {
    let mut basket = Basket::default();
    basket.stage_state(&id(3), "success", "", &["done a".into(), String::new()]);
    assert_eq!(basket.acts[0]["evidence"], json!(["done a", ""]));
    basket.stage_state(&id(3), "wip", "", &["ignored".into()]);
    assert!(basket.acts[0].get("evidence").is_none());
}

/// The form is checked by the propose door's own shape check.
#[test]
fn the_form_builds_a_checked_add() {
    let mut f = Form {
        title: "sync".into(),
        assignees: vec!["mara".into(), "walter".into()],
        acceptance: vec!["one".into(), "  ".into()],
        mode: 2,
        start: "2026-10-12T09:00".into(),
        end: "2026-10-12T10:00".into(),
        freq: "weekly".into(),
        interval: "2".into(),
        ..Form::default()
    };
    let act = form_act(&f).expect("valid");
    assert_eq!(act["repeat"], json!({"freq": "weekly", "interval": 2}));
    assert_eq!(act["acceptance"], json!(["one"]), "blank items drop");
    f.due = "2026-8-1".into();
    assert!(form_act(&f).is_err(), "a date must round-trip");
    f.due = String::new();
    f.assignees.clear();
    assert!(form_act(&f).is_err(), "at least one assignee");
}

#[test]
fn mine_lists_the_actions_first_then_my_other_tasks() {
    let l = Lexicon::en();
    let mut f = feed();
    f.actions = vec![
        WakeAction::TaskWip { task: id(1) },
        WakeAction::Poke { by: "walter".into(), since: 0 },
    ];
    let (actions, rest) = mine(&l, &f);
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].text, "api");
    assert_eq!(actions[1].kind, "poke");
    let rest: Vec<String> = rest.iter().map(|r| r.key.to_string()).collect();
    assert!(!rest.contains(&id(1)), "already listed");
    // created by mara: every task of the fixture
    assert!(rest.contains(&id(2)) && rest.contains(&id(4)));
}

#[test]
fn the_drill_in_shows_both_link_directions_and_the_checklist() {
    let l = Lexicon::en();
    let f = feed();
    let d = detail(&l, &f, &Basket::default(), &id(1)).expect("a task");
    assert!(d.blocked_by.is_empty());
    assert_eq!(d.prereq_for.len(), 1);
    assert_eq!(d.prereq_for[0].key.as_str(), id(2));
    let moves: Vec<String> = d.moves.iter().map(|m| m.key.to_string()).collect();
    assert_eq!(moves, ["todo", "success", "fail", "cancelled"]);
    let c = detail(&l, &f, &Basket::default(), &id(2)).expect("a task");
    assert_eq!(c.blocked_by[0].key.as_str(), id(1));
    assert_eq!(c.head.progress, "0/1 prerequisites done");
    let docs = detail(&l, &f, &Basket::default(), &id(3)).expect("a task");
    assert_eq!(docs.checks.len(), 2);
    assert!(!docs.checks[0].done);
}

/// §8: notifications follow `wake_on` and a burst is one toast.
#[test]
fn notices_coalesce_and_follow_wake_on() {
    let l = Lexicon::en();
    let all: Vec<String> = molt_core::kanban_wake::WAKE_REASONS.iter().map(|r| (*r).to_string()).collect();
    let mut n = Notices::default();
    assert!(n.note("kanban", &all), "the first trigger arms");
    assert!(!n.note("vote_pending", &all), "the rest ride along");
    assert!(!n.note_poke("walter", &all));
    assert_eq!(n.take(&l, "poked you").as_deref(), Some("walter poked you · Vote waiting · Kanban changed"));
    assert_eq!(n.take(&l, "x"), None);
    let only_votes = vec!["vote_pending".to_string()];
    assert!(!n.note("kanban", &only_votes), "switched off");
    let starts = [WakeAction::TaskStart { task: id(4), occurrence: None, late: false }];
    assert!(n.note_starts(&starts, &all));
    n.take(&l, "x");
    assert!(!n.note_starts(&starts, &all), "a start toasts once");
}
