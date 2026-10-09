// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban projections (`kanban_workflows.md` §8) over a board folded
//! and derived by molt-core, exactly as the engine serves it.

use std::collections::BTreeSet;

use molt_core::kanban_dates::derive;
use molt_core::kanban_fold::{kanban_precheck, short_id};
use molt_core::kanban_review::board_view;
use molt_core::kanban_view::type_slot;
use molt_core::kanban_wake::WakeAction;
use molt_core::SurfaceSnapshot;
use serde_json::{json, Value};

use crate::i18n::Lexicon;
use crate::kanban::*;

pub(super) fn id(n: u32) -> String {
    format!("{n:032x}")
}

pub(super) fn add(n: u32, title: &str, who: &[&str], extra: Value) -> Value {
    let mut v = json!({"act": "add", "id": id(n), "creator": "mara", "title": title, "assignees": who});
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    v
}

pub(super) fn st(n: u32, to: &str, note: Option<&str>) -> Value {
    let mut v = json!({"act": "state", "id": id(n), "to": to});
    if let Some(note) = note {
        v["note"] = Value::from(note);
    }
    v
}

pub(super) fn cs(ops: Vec<Value>) -> Value {
    json!({"op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": ops})
}

pub(super) fn feed_with(applied: Vec<Value>, pending: Value, me: &str) -> KanbanFeed {
    let seats = ["mara", "walter", "bot"];
    let set: BTreeSet<String> = seats.iter().map(|s| (*s).to_string()).collect();
    let board = molt_core::kanban_fold::kanban_fold(&applied, &set);
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
    KanbanFeed::new(snap, Vec::new(), seats.iter().map(|s| (*s).to_string()).collect(), me.to_string())
}

/// 1 api (mara, wip); 2 client blocked by 1; 3 docs open (Bug); 4 meet
/// timed once; 5 broken (fail); 6 waits on 5 (stuck); 7 done long ago.
pub(super) fn feed() -> KanbanFeed {
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
    assert_eq!(b.counts, [4, 1, 0], "a shadow is not on the board");
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
    basket.add(form_act(&l, &form).expect("a valid add"));
    let r = review(&l, &f, &basket);
    assert!(r.acts.iter().any(|a| a == "new 1 add: write"), "{:?}", r.acts);
    assert!(r.acts.iter().any(|a| a.starts_with(&short_id(&id(2)))), "{:?}", r.acts);
    assert_eq!(
        r.advisories.iter().filter(|a| a.starts_with("would void now: ")).count(),
        1,
        "{:?}",
        r.advisories
    );
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
    let act = form_act(&Lexicon::en(), &f).expect("valid");
    assert_eq!(act["repeat"], json!({"freq": "weekly", "interval": 2}));
    assert_eq!(act["acceptance"], json!(["one"]), "blank items drop");
    f.due = "2026-8-1".into();
    assert!(form_act(&Lexicon::en(), &f).is_err(), "a date must round-trip");
    f.due = String::new();
    f.assignees.clear();
    assert!(form_act(&Lexicon::en(), &f).is_err(), "at least one assignee");
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

/// A rescued add comes back without its minted id: the engine mints a
/// fresh one, and the changeset's own references follow through a ref.
#[test]
fn a_rescued_add_gets_a_fresh_id_and_its_references_follow() {
    let declined = cs(vec![
        add(20, "new", &["mara"], json!({})),
        add(21, "after", &["mara"], json!({"blocked_by": [id(20), id(1)]})),
        st(20, "wip", None),
    ]);
    let mut basket = Basket::default();
    basket.rescue(&declined);
    basket.rescue(&declined);
    let text = serde_json::to_string(&basket.acts).expect("json");
    assert!(!text.contains(&id(20)) && !text.contains(&id(21)), "{text}");
    assert!(!text.contains("creator"), "{text}");
    assert_eq!(basket.acts[1]["blocked_by"], json!(["@r1", id(1)]));
    assert_eq!(basket.acts[2]["id"], json!("@r1"));
    assert_eq!(basket.acts[3]["ref"], json!("r3"), "a second rescue takes new refs");
    let r = review(&Lexicon::en(), &feed(), &basket);
    assert!(!r.advisories.iter().any(|a| a.starts_with("would void")), "{:?}", r.advisories);
}

/// §2.4: the legend names the types the view lists, the type filter aside.
#[test]
fn the_legend_survives_a_type_filter() {
    let l = Lexicon::en();
    let mut applied = feed().snap.applied;
    applied.push(cs(vec![add(30, "feat", &["mara"], json!({"type": "feature"}))]));
    let f = feed_with(applied, json!([]), "mara");
    let on: BTreeSet<String> = ["type:bug".to_string()].into();
    let b = board(&l, &f, &on, &Basket::default());
    let labels: Vec<String> = b.legend.iter().map(|t| t.label.to_string()).collect();
    assert_eq!(labels, ["bug", "feature"]);
    assert!(b.legend[0].on && !b.legend[1].on);
}

/// The drop and move table is the fold's own (§2.2): every pair agrees
/// with the precheck.
#[test]
fn the_move_table_matches_the_fold() {
    let states = ["todo", "wip", "success", "fail", "cancelled"];
    for timed in [false, true] {
        for from in states {
            for to in states {
                let extra = if timed {
                    json!({"when": {"start": "2026-10-20", "end": "2026-10-20"}})
                } else {
                    json!({})
                };
                let mut ops = vec![add(1, "t", &["mara"], extra)];
                let path: &[&str] = match from {
                    "todo" => &[],
                    "wip" => &["wip"],
                    "success" => &["wip", "success"],
                    "fail" => &["wip", "fail"],
                    _ => &["cancelled"],
                };
                let seats: BTreeSet<String> = ["mara".to_string()].into();
                for p in path {
                    ops.push(st(1, p, Some("n")));
                }
                let board = molt_core::kanban_fold::kanban_fold(&[cs(ops)], &seats);
                let fold_ok = kanban_precheck(&board, &cs(vec![st(1, to, Some("n"))]), &seats).is_ok();
                assert_eq!(legal_move(from, timed, false, to), fold_ok, "{from} -> {to} timed={timed}");
            }
        }
    }
}

/// Each basket row shows its own act, even two acts on one task.
#[test]
fn each_basket_row_renders_its_own_act() {
    let mut basket = Basket::default();
    basket.acts.push(json!({"act": "set", "id": id(3), "fields": {"size": "L"}}));
    basket.stage_state(&id(3), "wip", "", &[]);
    let r = review(&Lexicon::en(), &feed(), &basket);
    assert_eq!(r.rows.len(), 2);
    assert!(r.rows[0].contains("size"), "{:?}", r.rows);
    assert!(r.rows[1].contains("state"), "{:?}", r.rows);
}

/// §2.4: a type only a hidden task carries is not in the legend.
#[test]
fn the_legend_skips_types_the_view_hides() {
    let l = Lexicon::en();
    let mut applied = feed().snap.applied;
    applied.insert(1, cs(vec![add(31, "gone", &["mara"], json!({"type": "chore"})), st(31, "cancelled", Some("n"))]));
    let f = feed_with(applied, json!([]), "mara");
    let b = board(&l, &f, &BTreeSet::new(), &Basket::default());
    let labels: Vec<String> = b.legend.iter().map(|t| t.label.to_string()).collect();
    assert_eq!(labels, ["bug"]);
    let on: BTreeSet<String> = ["closed".to_string()].into();
    let b = board(&l, &f, &on, &Basket::default());
    assert!(b.legend.iter().any(|t| t.label == "chore"), "closed lists it");
}

/// Two pending changesets on one card: the lower id badges it, every push.
#[test]
fn the_vote_badge_is_the_lowest_pending_id() {
    let l = Lexicon::en();
    let p = |n: u64| json!({
        "id": n, "surface": "quests", "approvals": 0, "threshold": 2, "state": "proposed",
        "payload": cs(vec![st(1, "success", None)]),
    });
    let f = feed_with(feed().snap.applied, json!([p(12), p(10)]), "mara");
    assert_eq!(vote_badges(&l, &f.snap).get(&id(1)).map(|v| v.1), Some(10));
}

/// §8: every filter of the table is offered, grouped in rows.
#[test]
fn the_filter_rows_offer_the_whole_table() {
    let l = Lexicon::en();
    let f = feed();
    let rows = filter_rows(&l, &f, &BTreeSet::new());
    let keys: Vec<Vec<String>> =
        rows.iter().map(|r| r.iter().map(|p| p.key.to_string()).collect()).collect();
    assert_eq!(keys[0], ["mine", "to_act_on", "starting_now", "created_by_me", "needs_my_vote"]);
    assert_eq!(
        keys[1],
        ["blocked", "stuck", "overdue", "date_conflict", "floating", "timed", "size:XS", "size:S", "size:M", "size:L", "size:XL", "size:XXL"]
    );
    assert_eq!(keys[2], ["todo", "wip", "success", "fail", "cancelled", "closed"]);
    assert_eq!(keys[3], ["seat:walter", "seat:bot"], "another seat's view");
    let on: BTreeSet<String> = ["size:M".to_string()].into();
    assert_eq!(ids(&board(&l, &f, &on, &Basket::default()).cols[1]), [id(1)]);
    assert!(board(&l, &f, &on, &Basket::default()).cols[0].is_empty());
    let on: BTreeSet<String> = ["mine".to_string(), "seat:walter".to_string()].into();
    let b = board(&l, &f, &on, &Basket::default());
    assert_eq!(ids(&b.cols[0]), [id(2), id(6), id(4)], "walter's tasks");
    let rows = filter_rows(&l, &f, &on);
    assert!(rows[3][0].on);
}

/// An engine event raises the notification its `wake_on` reason names.
#[test]
fn engine_events_map_to_notice_reasons() {
    use molt_core::{Event, ProposalId, Surface};
    let proposed = |by: &str| Event::Proposed { id: ProposalId(1), surface: Surface::Memory, by: by.into() };
    assert_eq!(notice_reason(&proposed("walter"), "mara"), Some(("vote_pending", String::new())));
    assert_eq!(notice_reason(&proposed("mara"), "mara"), None, "an own proposal");
    let applied = |surface| Event::Applied { id: ProposalId(1), surface };
    assert_eq!(notice_reason(&applied(Surface::Quests), "mara"), Some(("kanban", String::new())));
    assert_eq!(notice_reason(&applied(Surface::Memory), "mara"), None);
    let poke = |by: &str, to: &str| Event::Poked { by: by.into(), to: to.into() };
    assert_eq!(notice_reason(&poke("walter", "mara"), "mara"), Some(("poked", "walter".to_string())));
    assert_eq!(notice_reason(&poke("mara", "walter"), "mara"), None, "the sender's echo");
}

/// The form's errors speak the reader's language.
#[test]
fn a_form_number_error_is_localized() {
    let f = Form {
        title: "sync".into(),
        assignees: vec!["mara".into()],
        mode: 2,
        start: "2026-10-12T09:00".into(),
        end: "2026-10-12T10:00".into(),
        freq: "weekly".into(),
        interval: "x".into(),
        ..Form::default()
    };
    assert_eq!(form_act(&Lexicon::de(), &f), Err("Alle: keine Zahl".to_string()));
    assert_eq!(form_act(&Lexicon::en(), &f), Err("Every: not a number".to_string()));
}

/// After a Propose only the acts it sent leave, wherever they now sit.
#[test]
fn settling_a_propose_removes_exactly_what_was_sent() {
    let mut basket = Basket::default();
    basket.stage_state(&id(1), "success", "", &[]);
    basket.stage_state(&id(3), "wip", "", &[]);
    basket.summary = "s".into();
    let sent = basket.acts.clone();
    basket.acts.remove(0);
    basket.stage_state(&id(2), "wip", "", &[]);
    basket.settle(&sent);
    assert_eq!(basket.acts, [st(2, "wip", None)]);
    assert_eq!(basket.summary, "");
}
