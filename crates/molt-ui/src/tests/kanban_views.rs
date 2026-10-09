// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban views beside the board (`kanban_workflows.md` §8): the
//! dependency tree, the deadline timeline and the calendar.

use std::collections::BTreeSet;

use chrono::NaiveDate;
use serde_json::json;

use super::kanban::{add, cs, feed, feed_with, id};
use crate::i18n::Lexicon;
use crate::kanban::{Basket, KanbanFeed};
use crate::kanban_views::*;

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("a date")
}

/// 1 a; 2 b and 3 c blocked by a; 4 d blocked by b and c, due 20 Oct;
/// 5 e floating, no date; 6 t timed on 14 Oct.
fn diamond() -> KanbanFeed {
    feed_with(
        vec![cs(vec![
            add(1, "a", &["mara"], json!({"type": "Bug"})),
            add(2, "b", &["walter"], json!({"blocked_by": [id(1)]})),
            add(3, "c", &["bot"], json!({"blocked_by": [id(1)]})),
            add(4, "d", &["mara"], json!({"blocked_by": [id(2), id(3)], "due": "2026-10-20"})),
            add(5, "e", &["mara"], json!({})),
            add(6, "t", &["mara"], json!({"when": {"start": "2026-10-14T09:00", "end": "2026-10-14T10:00"}})),
        ])],
        json!([]),
        "mara",
    )
}

fn none() -> BTreeSet<String> {
    BTreeSet::new()
}

fn rows(d: &Deps) -> Vec<(String, i32)> {
    d.rows.iter().map(|r| (r.id.to_string(), r.depth)).collect()
}

/// §8: the top level is what nothing waits for; collapsed until opened.
#[test]
fn dependencies_start_at_the_tasks_nothing_waits_for() {
    let l = Lexicon::en();
    let d = dependencies(&l, &diamond(), &none(), &none());
    let top: BTreeSet<String> = d.rows.iter().map(|r| r.id.to_string()).collect();
    assert_eq!(top, [id(4), id(5), id(6)].into_iter().collect());
    assert!(d.rows.iter().all(|r| r.depth == 0 && !r.open));
    let dr = d.rows.iter().find(|r| r.id == id(4).as_str()).expect("d");
    assert!(dr.kids);
    assert_eq!(dr.progress.as_str(), "0/3", "all prerequisites to any depth");
    assert_eq!(dr.needed.as_str(), "needed by 20 Oct");
}

/// §8: expandable to any depth; a shared prerequisite sits under every
/// parent, marked "also for …".
#[test]
fn a_shared_prerequisite_shows_under_each_parent() {
    let l = Lexicon::en();
    let open: BTreeSet<String> =
        [id(4), format!("{}>{}", id(4), id(2)), format!("{}>{}", id(4), id(3))].into_iter().collect();
    let d = dependencies(&l, &diamond(), &none(), &open);
    let tree: Vec<(String, i32)> = rows(&d).into_iter().take(5).collect();
    assert_eq!(
        tree,
        [(id(4), 0), (id(2), 1), (id(1), 2), (id(3), 1), (id(1), 2)],
        "{:?}",
        rows(&d)
    );
    let under_b = &d.rows[2];
    assert_eq!(under_b.key.as_str(), format!("{}>{}>{}", id(4), id(2), id(1)));
    assert_eq!(under_b.also.as_str(), "also for c");
    assert_eq!(d.rows[4].also.as_str(), "also for b");
    assert!(d.rows[0].also.is_empty(), "a top has no parent to be 'also' for");
}

/// §8: a filter keeps its matches and, muted, what they are a
/// prerequisite for; while filtering the tree starts open.
#[test]
fn a_filter_keeps_matches_with_their_dependents_muted() {
    let l = Lexicon::en();
    let on: BTreeSet<String> = ["mine".to_string()].into();
    // as walter: only b is his
    let f = {
        let mut f = diamond();
        f.me = "walter".into();
        if let Some(b) = f.snap.board.as_mut() {
            b["me"] = json!("walter");
        }
        f
    };
    let d = dependencies(&l, &f, &on, &none());
    assert_eq!(rows(&d), [(id(4), 0), (id(2), 1)]);
    assert!(d.rows[0].muted, "d is context");
    assert!(!d.rows[1].muted, "b is the match");
    assert!(d.rows[0].open);
}

/// §8 `plan`: dated trees first, a task once under its first parent, the
/// rest in the "no date" lane; markers at `needed_by`, blocks at `when`.
#[test]
fn the_plan_places_each_task_once_with_its_date() {
    let l = Lexicon::en();
    let p = plan(&l, &diamond(), &none(), &none());
    let ids: Vec<String> = p.rows.iter().map(|r| r.id.to_string()).collect();
    let nodate = p.nodate.expect("e has no date");
    assert_eq!(ids[nodate], id(5));
    assert_eq!(nodate, ids.len() - 1, "the lane is last");
    let d = ids.iter().position(|x| *x == id(4)).expect("d");
    assert_eq!(&ids[d..d + 4], [id(4), id(2), id(1), id(3)], "a sits once, under b");
    assert_eq!(ids.iter().filter(|x| **x == id(1)).count(), 1);
    let bar = |n: u32| {
        let row = i32::try_from(ids.iter().position(|x| *x == id(n)).expect("row")).expect("i32");
        p.bars.iter().find(|b| b.row == row).cloned()
    };
    assert!(bar(4).expect("d").marker, "floating: a marker at needed_by");
    assert!(bar(1).expect("a").marker, "a inherits d's needed_by");
    let t = bar(6).expect("t");
    assert!(!t.marker && t.x1 > t.x0, "timed: a block");
    assert!(bar(5).is_none());
    assert_eq!(p.arrows.len(), 4, "a→b, a→c, b→d, c→d");
    assert!(p.today > 0.0 && p.today < 1.0);
    assert!(!p.ticks.is_empty());
    assert_eq!(p.rows[d].date.as_str(), "needed by 20 Oct");
}

/// §8: a row collapses over its prerequisites.
#[test]
fn a_plan_row_collapses() {
    let l = Lexicon::en();
    let shut: BTreeSet<String> = [id(4)].into();
    let p = plan(&l, &diamond(), &none(), &shut);
    let ids: BTreeSet<String> = p.rows.iter().map(|r| r.id.to_string()).collect();
    assert!(!ids.contains(&id(1)) && !ids.contains(&id(2)));
    let d = p.rows.iter().find(|r| r.id == id(4).as_str()).expect("d");
    assert!(d.kids && !d.open);
}

/// §8: overdue, date conflicts and stuck are red on the timeline.
#[test]
fn the_plan_marks_stuck_red() {
    let l = Lexicon::en();
    let p = plan(&l, &feed(), &none(), &none());
    let stuck = p.rows.iter().find(|r| r.id == id(6).as_str()).expect("waits");
    assert!(stuck.bad);
}

/// 1 sync weekly from Mon 12 Oct 09:00; 2 offsite all-day 14-15 Oct;
/// 3 floating with a due date.
fn timed() -> KanbanFeed {
    feed_with(
        vec![cs(vec![
            add(1, "sync", &["mara"], json!({
                "when": {"start": "2026-10-12T09:00", "end": "2026-10-12T10:00"},
                "repeat": {"freq": "weekly"}
            })),
            add(2, "offsite", &["mara"], json!({"when": {"start": "2026-10-14", "end": "2026-10-15"}})),
            add(3, "float", &["mara"], json!({"due": "2026-10-16"})),
        ])],
        json!([]),
        "mara",
    )
}

/// §8 `calendar`: whole weeks around the month, timed tasks only, a
/// series expanded, a multi-day block on each of its days.
#[test]
fn the_month_shows_only_timed_work() {
    let l = Lexicon::en();
    let c = calendar(&l, &timed(), &Basket::default(), day(2026, 10, 12), false);
    assert_eq!(c.days.len(), 35);
    assert_eq!(c.days[0].date.as_str(), "2026-09-28");
    assert_eq!(c.title, "October 2026");
    let on = |n: u32| -> Vec<String> {
        c.blocks.iter().filter(|b| b.id == id(n).as_str()).map(|b| c.days[usize::try_from(b.day).expect("day")].date.to_string()).collect()
    };
    assert_eq!(on(1), ["2026-10-12", "2026-10-19", "2026-10-26"]);
    assert_eq!(on(2), ["2026-10-14", "2026-10-15"]);
    assert!(on(3).is_empty(), "floating work never appears");
    assert!(c.days.iter().any(|d| d.today && d.date == "2026-10-12"));
    assert!(!c.days[0].in_month);
}

/// Week: a timed block knows where it sits in its day.
#[test]
fn the_week_places_blocks_in_the_day() {
    let l = Lexicon::en();
    let c = calendar(&l, &timed(), &Basket::default(), day(2026, 10, 14), true);
    assert_eq!(c.days.len(), 7);
    assert_eq!(c.days[0].date.as_str(), "2026-10-12");
    let sync = c.blocks.iter().find(|b| b.id == id(1).as_str()).expect("sync");
    assert_eq!(sync.day, 0);
    assert!((sync.start - 0.375).abs() < 1e-4 && (sync.end - 10.0 / 24.0).abs() < 1e-4);
    assert_eq!(sync.time.as_str(), "09:00");
    assert!(c.blocks.iter().filter(|b| b.id == id(2).as_str()).all(|b| b.all_day));
}

/// §8: a drag stages a `set` - the board's block stays, a shadow waits at
/// the target; a series occurrence lands in `moved`.
#[test]
fn a_dragged_block_stages_a_move() {
    let l = Lexicon::en();
    let f = timed();
    let mut basket = Basket::default();
    let c = calendar(&l, &f, &basket, day(2026, 10, 12), false);
    let i = c.blocks.iter().position(|b| b.id == id(2).as_str()).expect("offsite");
    let src = &c.src[usize::try_from(c.blocks[i].src).expect("draggable")];
    let (task, fields) = drop_fields(&f, &basket, src, day(2026, 10, 21), None).expect("a move");
    assert_eq!(task, id(2));
    assert_eq!(fields["when"], json!({"start": "2026-10-21", "end": "2026-10-22"}), "length kept");
    basket.stage_set(&task, fields);
    assert_eq!(basket.acts.len(), 1);

    let w = calendar(&l, &f, &basket, day(2026, 10, 19), true);
    let i = w.blocks.iter().position(|b| b.id == id(1).as_str() && !b.shadow).expect("sync on the 19th");
    let src = &w.src[usize::try_from(w.blocks[i].src).expect("draggable")];
    let (task, fields) = drop_fields(&f, &basket, src, day(2026, 10, 20), Some(14 * 60)).expect("a move");
    assert_eq!(
        fields["moved"],
        json!({"2026-10-19": {"start": "2026-10-20T14:00", "end": "2026-10-20T15:00"}})
    );
    basket.stage_set(&task, fields);
    let w = calendar(&l, &f, &basket, day(2026, 10, 19), true);
    let sync: Vec<(i32, bool)> = w.blocks.iter().filter(|b| b.id == id(1).as_str()).map(|b| (b.day, b.shadow)).collect();
    assert_eq!(sync, [(0, false), (1, true)], "the block stays, the shadow waits");
    let offsite: Vec<(i32, bool)> = w.blocks.iter().filter(|b| b.id == id(2).as_str()).map(|b| (b.day, b.shadow)).collect();
    assert_eq!(offsite, [(2, true), (3, true)], "the staged offsite on 21-22 Oct");
    assert!(drop_fields(&f, &basket, src, day(2026, 10, 19), Some(9 * 60)).is_none(), "no move, no act");
}

/// §8: a free slot opens the form with that slot (length in the form).
#[test]
fn a_free_slot_prefills_the_form() {
    assert_eq!(slot_times(day(2026, 10, 14), None), ("2026-10-14".to_string(), "2026-10-14".to_string()));
    assert_eq!(
        slot_times(day(2026, 10, 14), Some(9 * 60)),
        ("2026-10-14T09:00".to_string(), "2026-10-14T10:00".to_string())
    );
}

/// §2.3: Organization › Status shows the one clock everybody reads.
#[test]
fn the_status_clock_is_utc() {
    let t = day(2026, 10, 9).and_hms_opt(14, 32, 59).expect("a time");
    assert_eq!(utc_clock(t), "2026-10-09 14:32 UTC");
}
