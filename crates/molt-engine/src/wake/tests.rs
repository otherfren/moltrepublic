//! S2 wake keystones (`docs_archive/kanban/kanban_workflows.md` §9).

use molt_core::kanban_wake::WakeAction;
use molt_core::{ChainChange, ChannelRef, Command, MoltError, Reply, Surface};
use serde_json::{json, Value};

use crate::chain::test_support::{genesis_seat, Builder};

/// Mon 2026-10-12, 12:00 UTC.
const NOON: u64 = 1_791_806_400;
const MIN: u64 = 60;

fn tid(n: u32) -> String {
    format!("{n:08x}{}", "0".repeat(24))
}

fn cs(ops: Vec<Value>) -> Value {
    json!({"op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": ops})
}

fn add(n: u32, assignees: &[&str], extra: Value) -> Value {
    let mut v = json!({"act": "add", "id": tid(n), "creator": "mara", "title": "t", "assignees": assignees});
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    v
}

fn st(n: u32, to: &str) -> Value {
    json!({"act": "state", "id": tid(n), "to": to})
}

fn republic() -> Builder {
    Builder::new_with_features(&["mara", "walter", "bot"], 2, &["quests"], false)
}

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

/// A seat with a wake command and no rest between wakes.
fn seat(member: &str, b: &Builder, at: u64) -> crate::State {
    let mut s = genesis_seat(member, b, b.blocks.clone());
    s.session.active_workspace = "ws".to_string();
    s.presence.clock_override = Some(at);
    s.session.settings.poke_wake_command = "true".to_string();
    s.session.settings.wake_min_interval_secs = 0;
    s
}

/// Tick until the running wake was handed back.
fn settle(s: &mut crate::State) {
    super::await_idle(s);
    s.wake_tick();
}

fn meeting(n: u32, extra: Value) -> Value {
    let mut v = add(
        n,
        &["mara", "walter"],
        json!({"when": {"start": "2026-10-12T14:00", "end": "2026-10-12T15:00"}}),
    );
    if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    v
}

#[test]
fn a_trigger_during_a_running_wake_fires_once_after_it_with_both_reasons() {
    let dir = tempfile::tempdir().expect("tmp");
    let hold = dir.path().join("hold");
    let marker = dir.path().join("marker");
    std::fs::write(&hold, b"").expect("hold");
    let mut s = crate::tests::plain_state();
    s.presence.clock_override = Some(NOON);
    s.session.settings.wake_min_interval_secs = 0;
    s.session.settings.poke_wake_command = format!(
        "echo \"$MOLT_WAKE_REASON $MOLT_WAKE_ACTIONS\" >> '{}'; while [ -e '{}' ]; do sleep 0.02; done",
        marker.display(),
        hold.display()
    );
    s.wake_trigger("vote_pending", "peer-1");
    assert_eq!(s.wake.log, ["vote_pending"]);
    s.wake_trigger("poked", "peer-1");
    s.wake_trigger("kanban", "");
    s.wake_trigger("poked", "peer-2");
    s.wake_tick();
    assert_eq!(s.wake.log.len(), 1, "nothing fires beside a running wake");

    std::fs::remove_file(&hold).expect("release");
    settle(&mut s);
    assert_eq!(s.wake.log, ["vote_pending", "kanban,poked"], "one wake, sorted reasons");
    settle(&mut s);
    assert_eq!(s.wake.log.len(), 2, "and only one");
    let lines = std::fs::read_to_string(&marker).expect("marker");
    assert_eq!(lines.lines().collect::<Vec<_>>(), ["vote_pending 0", "kanban,poked 0"]);
}

#[test]
fn the_interval_is_honoured_for_every_reason() {
    let mut s = crate::tests::plain_state();
    s.session.settings.poke_wake_command = "true".to_string();
    s.presence.clock_override = Some(NOON);
    s.wake_trigger("kanban", "");
    settle(&mut s);
    s.presence.clock_override = Some(NOON + 100);
    s.wake_trigger("poked", "peer-1");
    s.presence.clock_override = Some(NOON + 299);
    s.wake_tick();
    assert_eq!(s.wake.log, ["kanban"], "default rest: 300 s");
    s.presence.clock_override = Some(NOON + 300);
    s.wake_tick();
    assert_eq!(s.wake.log, ["kanban", "poked"]);
}

#[test]
fn wake_on_switches_a_reason_off() {
    let mut s = crate::tests::plain_state();
    s.session.settings.poke_wake_command = "true".to_string();
    s.session.settings.wake_on = vec!["poked".to_string()];
    s.wake_trigger("kanban", "");
    s.wake_tick();
    assert!(s.wake.log.is_empty());
    s.wake_trigger("poked", "peer-1");
    assert_eq!(s.wake.log, ["poked"]);
}

#[test]
fn every_node_wakes_on_an_applied_kanban_changeset() {
    let mut b = republic();
    let block = commit(&mut b, 1, cs(vec![add(1, &["mara"], json!({}))]));
    for m in ["mara", "walter", "bot"] {
        let mut s = seat(m, &republic(), NOON);
        s.receive_block(block.clone());
        assert_eq!(s.wake.log, ["kanban"], "{m}");
    }
}

#[test]
fn one_task_start_per_occurrence_on_assignee_nodes_only() {
    let mut b = republic();
    commit(&mut b, 1, cs(vec![meeting(7, json!({}))]));
    let at = NOON + 110 * MIN; // 13:50, lead 10
    for m in ["mara", "walter", "bot"] {
        let mut s = seat(m, &b, at - 1);
        s.wake_tick();
        assert!(s.wake.log.is_empty(), "{m}: not yet");
        s.presence.clock_override = Some(at);
        s.wake_tick();
        settle(&mut s);
        s.presence.clock_override = Some(at + 30 * MIN);
        s.wake_tick();
        settle(&mut s);
        let want: &[&str] = if m == "bot" { &[] } else { &["task_start"] };
        assert_eq!(s.wake.log, want, "{m}");
    }
}

#[test]
fn a_missed_start_under_a_day_fires_once_as_late() {
    let mut b = republic();
    commit(&mut b, 1, cs(vec![meeting(7, json!({}))]));
    let late = NOON + 5 * 60 * MIN; // 17:00, three hours past
    let mut s = seat("mara", &b, late);
    s.wake_tick();
    assert_eq!(s.wake.log, ["task_start"]);
    assert_eq!(
        s.read_actions(),
        [WakeAction::TaskStart { task: tid(7), occurrence: None, late: true }]
    );
    let fired: Vec<String> = s.wake.fired.iter().cloned().collect();

    // a restart: the sealed keys come back, nothing fires twice
    let mut again = seat("mara", &b, late + MIN);
    again.wake.adopt_fired(fired);
    again.wake_tick();
    assert!(again.wake.log.is_empty());

    let mut stale = seat("mara", &b, NOON + 26 * 60 * MIN);
    stale.wake_tick();
    assert!(stale.wake.log.is_empty(), "a day late fires nothing");
}

#[test]
fn a_blocked_appointment_fires_nothing_and_is_listed_late_once_unblocked() {
    let mut b = republic();
    commit(
        &mut b,
        1,
        cs(vec![
            add(1, &["walter"], json!({})),
            meeting(2, json!({"blocked_by": [tid(1)]})),
        ]),
    );
    let mut s = seat("mara", &b, NOON + 2 * 60 * MIN);
    s.wake_tick();
    assert!(s.wake.log.is_empty(), "blocked at its start");
    assert!(s.read_actions().is_empty());

    for (id, to) in [(2, "wip"), (3, "success")] {
        let block = commit(&mut b, id, cs(vec![st(1, to)]));
        s.receive_block(block);
        settle(&mut s);
    }
    s.presence.clock_override = Some(NOON + 3 * 60 * MIN);
    s.wake_tick();
    assert_eq!(s.wake.log, ["kanban", "kanban"], "the unblock wakes as kanban, no task_start");
    assert_eq!(
        s.read_actions(),
        [WakeAction::TaskStart { task: tid(2), occurrence: None, late: true }]
    );
}

#[test]
fn read_actions_for_the_worked_example() {
    let mut b = republic();
    commit(
        &mut b,
        1,
        cs(vec![
            add(1, &["mara"], json!({})),
            add(5, &["walter"], json!({"due": "2026-10-28"})),
            add(2, &["walter"], json!({"blocked_by": [tid(1), tid(5)]})),
            add(3, &["bot"], json!({"blocked_by": [tid(1)], "due": "2026-10-20"})),
            add(4, &["mara"], json!({})),
            add(6, &["mara"], json!({"blocked_by": [tid(1), tid(2), tid(3)], "due": "2026-10-23"})),
        ]),
    );
    commit(&mut b, 2, cs(vec![st(1, "wip")]));
    let mut mara = seat("mara", &b, NOON);
    assert_eq!(
        mara.read_actions(),
        [WakeAction::TaskWip { task: tid(1) }, WakeAction::TaskStartable { task: tid(4) }]
    );
    let mut bot = seat("bot", &b, NOON);
    assert!(bot.read_actions().is_empty(), "C waits for A");

    let mut walter = seat("walter", &b, NOON);
    walter.session.settings.poke_enabled = true;
    let pid = crate::chain::test_support::propose(
        &mut mara,
        Surface::Quests,
        cs(vec![json!({"act": "set", "id": tid(4), "fields": {"size": "S"}})]),
    );
    let card = mara.proposals.get(&pid).cloned().expect("card");
    walter.proposals.insert(pid, card);
    walter.wake.proposed_at.insert(pid, NOON - 5);
    let me = walter.member();
    walter.presence.clock_override = Some(NOON + 1);
    walter.receive_poke("mara", &me);
    assert_eq!(
        walter.read_actions(),
        [
            WakeAction::Vote { proposal: pid, surface: Surface::Quests, since: NOON - 5 },
            WakeAction::TaskStartable { task: tid(5) },
            WakeAction::Poke { by: "mara".to_string(), since: NOON + 1 },
        ]
    );
}

#[test]
fn the_read_actions_command_answers_the_list() {
    let mut b = republic();
    commit(&mut b, 1, cs(vec![add(4, &["mara"], json!({}))]));
    let mut mara = seat("mara", &b, NOON);
    match mara.handle(Command::ReadActions).expect("read") {
        Reply::Actions { actions } => {
            assert_eq!(actions, [WakeAction::TaskStartable { task: tid(4) }]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_chat_read_answers_the_poke_through_every_door() {
    let b = republic();
    let doors = [
        Command::MarkRead { ids: Vec::new() },
        Command::MarkChannelRead { channel: ChannelRef::default(), up_to: String::new() },
        Command::ReadState { surface: Surface::Chat, channel: None, view: None },
    ];
    for door in doors {
        let mut walter = seat("walter", &b, NOON);
        walter.session.settings.poke_enabled = true;
        let me = walter.member();
        walter.receive_poke("mara", &me);
        let bad = Command::MarkChannelRead { channel: ChannelRef::default(), up_to: "zz".to_string() };
        assert!(walter.handle(bad).is_err());
        assert_eq!(walter.read_actions().len(), 1, "a refused read answers nothing");
        let name = format!("{door:?}");
        walter.handle(door).expect("read");
        assert!(walter.read_actions().is_empty(), "{name}");
    }
}

#[test]
fn test_wake_runs_the_command_past_wake_on_and_reports_its_exit() {
    let mut s = crate::tests::plain_state();
    assert!(matches!(s.cmd_test_wake(), Err(MoltError::Settings(_))), "no command");
    s.session.settings.poke_wake_command = "exit 3".to_string();
    s.session.settings.wake_on = Vec::new();
    s.wake_trigger("kanban", "");
    s.cmd_test_wake().expect("test wake");
    assert_eq!(s.wake.log, ["test"]);
    assert_eq!(s.session.wake_test, "started");
    settle(&mut s);
    assert_eq!(s.session.wake_test, "exit 3");

    s.wake.set_running(true);
    s.cmd_test_wake().expect("test wake");
    assert_eq!(s.session.wake_test, "waiting");
    s.wake.set_running(false);
    s.wake_tick();
    assert_eq!(s.wake.log, ["test", "test"], "past the interval too");
}

#[test]
fn coalescing_keeps_the_poker() {
    let dir = tempfile::tempdir().expect("tmp");
    let hold = dir.path().join("hold");
    let marker = dir.path().join("marker");
    std::fs::write(&hold, b"").expect("hold");
    let mut s = crate::tests::plain_state();
    s.presence.clock_override = Some(NOON);
    s.session.settings.wake_min_interval_secs = 0;
    s.session.settings.poke_wake_command = format!(
        "echo \"$MOLT_WAKE_REASON by=$MOLT_WAKE_BY\" >> '{}'; while [ -e '{}' ]; do sleep 0.02; done",
        marker.display(),
        hold.display()
    );
    s.wake_trigger("kanban", "");
    s.wake_trigger("poked", "peer-1");
    s.wake_trigger("kanban", "");
    s.wake_trigger("task_start", "");
    std::fs::remove_file(&hold).expect("release");
    settle(&mut s);
    settle(&mut s);
    let lines = std::fs::read_to_string(&marker).expect("marker");
    assert_eq!(lines.lines().collect::<Vec<_>>(), ["kanban by=", "kanban,poked,task_start by=peer-1"]);
}

#[test]
fn the_rest_counts_from_the_end_of_the_last_wake() {
    let dir = tempfile::tempdir().expect("tmp");
    let hold = dir.path().join("hold");
    std::fs::write(&hold, b"").expect("hold");
    let mut s = crate::tests::plain_state();
    s.presence.clock_override = Some(NOON);
    s.session.settings.wake_min_interval_secs = 300;
    s.session.settings.poke_wake_command =
        format!("while [ -e '{}' ]; do sleep 0.02; done", hold.display());
    s.wake_trigger("kanban", "");
    s.presence.clock_override = Some(NOON + 400);
    s.wake_trigger("poked", "peer-1");
    std::fs::remove_file(&hold).expect("release");
    settle(&mut s);
    assert_eq!(s.wake.log, ["kanban"], "a long run is no rest");
    s.presence.clock_override = Some(NOON + 699);
    s.wake_tick();
    assert_eq!(s.wake.log, ["kanban"]);
    s.presence.clock_override = Some(NOON + 700);
    s.wake_tick();
    assert_eq!(s.wake.log, ["kanban", "poked"]);
}

#[test]
fn a_start_moved_after_it_fired_fires_again_at_its_new_time() {
    let mut b = republic();
    commit(&mut b, 1, cs(vec![meeting(7, json!({}))]));
    let at = NOON + 110 * MIN; // 13:50, lead 10
    let mut s = seat("mara", &b, at);
    s.wake_tick();
    settle(&mut s);
    assert_eq!(s.wake.log, ["task_start"]);
    let moved = json!({"act": "set", "id": tid(7), "fields": {"when": {"start": "2026-10-12T16:00", "end": "2026-10-12T17:00"}}});
    let block = commit(&mut b, 2, cs(vec![moved]));
    s.presence.clock_override = Some(at + 5 * MIN);
    s.receive_block(block);
    settle(&mut s);
    assert_eq!(s.wake.log, ["task_start", "kanban"]);
    s.presence.clock_override = Some(NOON + 230 * MIN); // 15:50
    s.wake_tick();
    settle(&mut s);
    assert_eq!(s.wake.log, ["task_start", "kanban", "task_start"], "a new start is a new appointment");
    s.presence.clock_override = Some(NOON + 240 * MIN);
    s.wake_tick();
    settle(&mut s);
    assert_eq!(s.wake.log.len(), 3, "and fires once");
}
