//! The kanban board as a deterministic FOLD (`docs/kanban/kanban_workflows.md`
//! §2, §4): applied `kanban_ops` changesets in chain order over the empty
//! board. A changeset applies all-or-nothing; one that breaks a rule is
//! VOID - it still counts in `rev`, it changes nothing else. Same chain,
//! same seats, byte-identical board on every node.
//!
//! The roster seats come in as a parameter: this crate has no I/O.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde_json::{Map, Value};

use crate::kanban_calendar::{fmt_date, is_occurrence, parse_date, Repeat, When};
pub use crate::wiki_fold::VoidReason;

/// A task id: 32 lowercase hex chars (128 random bits, minted at propose).
pub type TaskId = String;

/// The governed lifecycle state (§2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskState {
    /// Not started.
    Todo,
    /// In progress.
    Wip,
    /// Done and accepted.
    Success,
    /// Tried and failed.
    Fail,
    /// No longer needed.
    Cancelled,
}

impl TaskState {
    /// Every state, in table order.
    pub const ALL: [TaskState; 5] = [
        TaskState::Todo,
        TaskState::Wip,
        TaskState::Success,
        TaskState::Fail,
        TaskState::Cancelled,
    ];

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Todo => "todo",
            TaskState::Wip => "wip",
            TaskState::Success => "success",
            TaskState::Fail => "fail",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// Parse the wire spelling.
    #[must_use]
    pub fn parse(s: &str) -> Option<TaskState> {
        TaskState::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// `success`, `fail` or `cancelled` - the archive.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Success | TaskState::Fail | TaskState::Cancelled
        )
    }

    /// A prerequisite in this state no longer blocks (§2.2).
    #[must_use]
    pub fn is_met(self) -> bool {
        matches!(self, TaskState::Success | TaskState::Cancelled)
    }
}

/// The optional T-shirt size (§2.1) - orientation only, nothing reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Size {
    /// Extra small.
    Xs,
    /// Small.
    S,
    /// Medium.
    M,
    /// Large.
    L,
    /// Extra large.
    Xl,
    /// Extra extra large.
    Xxl,
}

impl Size {
    /// Every size, smallest first.
    pub const ALL: [Size; 6] = [Size::Xs, Size::S, Size::M, Size::L, Size::Xl, Size::Xxl];

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Size::Xs => "XS",
            Size::S => "S",
            Size::M => "M",
            Size::L => "L",
            Size::Xl => "XL",
            Size::Xxl => "XXL",
        }
    }

    /// Parse the wire spelling.
    #[must_use]
    pub fn parse(s: &str) -> Option<Size> {
        Size::ALL.into_iter().find(|z| z.as_str() == s)
    }
}

/// One task (§2.1) as the fold leaves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// One line.
    pub title: String,
    /// The free type label, as proposed.
    pub kind: Option<String>,
    /// The seat that proposed the `add`.
    pub creator: String,
    /// The seats who do the work.
    pub assignees: Vec<String>,
    /// Orientation only.
    pub size: Option<Size>,
    /// Prerequisites: tasks that must succeed (or be cancelled) first.
    pub blocked_by: Vec<TaskId>,
    /// Deadline.
    pub due: Option<NaiveDate>,
    /// Not before.
    pub after: Option<NaiveDate>,
    /// Calendar placement.
    pub when: Option<When>,
    /// A recurring series' rule.
    pub repeat: Option<Repeat>,
    /// Occurrences removed from the series.
    pub skip: Vec<NaiveDate>,
    /// Occurrences given a new window, keyed by their original date.
    pub moved: BTreeMap<NaiveDate, When>,
    /// The governed state.
    pub state: TaskState,
    /// Why the last transition happened.
    pub note: Option<String>,
    /// Per acceptance criterion, how it was met (set by `succeed`).
    pub evidence: Vec<String>,
    /// What and why, markdown.
    pub description: Option<String>,
    /// Acceptance criteria.
    pub acceptance: Vec<String>,
    /// What is explicitly not part of it.
    pub out_of_scope: Vec<String>,
    /// The `rev` of the changeset that last set any field.
    pub touched_rev: u64,
}

impl Task {
    /// A recurring series (§3.2).
    #[must_use]
    pub fn is_series(&self) -> bool {
        self.repeat.is_some()
    }

    /// Timed once: an appointment that may close straight from `todo`.
    #[must_use]
    pub fn is_timed_once(&self) -> bool {
        self.when.is_some() && self.repeat.is_none()
    }

    /// The canonical JSON form; keys inserted sorted so the bytes do not
    /// depend on serde_json's map flavour.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let strs = |v: &[String]| Value::Array(v.iter().cloned().map(Value::String).collect());
        let mut m = Map::new();
        if !self.acceptance.is_empty() {
            m.insert("acceptance".into(), strs(&self.acceptance));
        }
        if let Some(a) = self.after {
            m.insert("after".into(), Value::from(fmt_date(a)));
        }
        m.insert("assignees".into(), strs(&self.assignees));
        if !self.blocked_by.is_empty() {
            m.insert("blocked_by".into(), strs(&self.blocked_by));
        }
        m.insert("creator".into(), Value::from(self.creator.clone()));
        if let Some(d) = &self.description {
            m.insert("description".into(), Value::from(d.clone()));
        }
        if let Some(d) = self.due {
            m.insert("due".into(), Value::from(fmt_date(d)));
        }
        if !self.evidence.is_empty() {
            m.insert("evidence".into(), strs(&self.evidence));
        }
        if !self.moved.is_empty() {
            let mut mv = Map::new();
            for (k, w) in &self.moved {
                mv.insert(fmt_date(*k), w.to_json());
            }
            m.insert("moved".into(), Value::Object(mv));
        }
        if let Some(n) = &self.note {
            m.insert("note".into(), Value::from(n.clone()));
        }
        if !self.out_of_scope.is_empty() {
            m.insert("out_of_scope".into(), strs(&self.out_of_scope));
        }
        if let Some(r) = &self.repeat {
            m.insert("repeat".into(), r.to_json());
        }
        if let Some(s) = self.size {
            m.insert("size".into(), Value::from(s.as_str()));
        }
        if !self.skip.is_empty() {
            m.insert(
                "skip".into(),
                Value::Array(
                    self.skip
                        .iter()
                        .map(|d| Value::from(fmt_date(*d)))
                        .collect(),
                ),
            );
        }
        m.insert("state".into(), Value::from(self.state.as_str()));
        m.insert("title".into(), Value::from(self.title.clone()));
        m.insert("touched_rev".into(), Value::from(self.touched_rev));
        if let Some(k) = &self.kind {
            m.insert("type".into(), Value::from(k.clone()));
        }
        if let Some(w) = &self.when {
            m.insert("when".into(), w.to_json());
        }
        Value::Object(m)
    }
}

/// The fold result (§2.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardState {
    /// Kanban changesets folded, void ones too.
    pub rev: u64,
    /// Every task, all states.
    pub tasks: BTreeMap<TaskId, Task>,
}

impl BoardState {
    /// The canonical JSON form: `{"rev", "tasks": {id: task}}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut tasks = Map::new();
        for (id, t) in &self.tasks {
            tasks.insert(id.clone(), t.to_json());
        }
        let mut m = Map::new();
        m.insert("rev".into(), Value::from(self.rev));
        m.insert("tasks".into(), Value::Object(tasks));
        Value::Object(m)
    }
}

/// The display form of an id: `#` + the first 8 hex.
#[must_use]
pub fn short_id(id: &str) -> String {
    format!("#{}", id.get(..8).unwrap_or(id))
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_ref_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

// ---------------------------------------------------------------- shape --

/// One field a `set` (or an `add`) assigns.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    Title(String),
    Kind(Option<String>),
    Assignees(Vec<String>),
    Size(Option<Size>),
    BlockedBy(Vec<TaskId>),
    Due(Option<NaiveDate>),
    After(Option<NaiveDate>),
    When(Option<When>),
    Repeat(Option<Repeat>),
    Skip(Vec<NaiveDate>),
    Moved(BTreeMap<NaiveDate, When>),
    Description(Option<String>),
    Acceptance(Vec<String>),
    OutOfScope(Vec<String>),
}

#[derive(Clone, Debug)]
enum Act {
    Add {
        id: Option<TaskId>,
        creator: Option<String>,
        fields: Vec<Field>,
    },
    Set {
        id: TaskId,
        fields: Vec<Field>,
    },
    State {
        id: TaskId,
        to: TaskState,
        note: Option<String>,
        evidence: Vec<String>,
    },
}

struct Changeset {
    acts: Vec<Act>,
}

const TASK_FIELDS: [&str; 14] = [
    "title",
    "type",
    "assignees",
    "size",
    "blocked_by",
    "due",
    "after",
    "when",
    "repeat",
    "skip",
    "moved",
    "description",
    "acceptance",
    "out_of_scope",
];

fn one_line(s: &str) -> bool {
    !s.trim().is_empty() && !s.contains(['\n', '\r'])
}

fn str_list(v: &Value, what: &str) -> Result<Vec<String>, String> {
    v.as_array()
        .ok_or_else(|| format!("{what}: not a list"))?
        .iter()
        .map(|x| {
            x.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{what}: not a list of strings"))
        })
        .collect()
}

fn line_list(v: &Value, what: &str) -> Result<Vec<String>, String> {
    let items = str_list(v, what)?;
    if items.iter().any(|s| !one_line(s)) {
        return Err(format!("{what}: an item is empty or not one line"));
    }
    Ok(items)
}

fn date_of(v: &Value, what: &str) -> Result<NaiveDate, String> {
    v.as_str()
        .and_then(parse_date)
        .ok_or_else(|| format!("{what}: not YYYY-MM-DD"))
}

/// Parse one task field; `null` clears an optional one.
fn parse_field(key: &str, v: &Value) -> Result<Field, String> {
    let null = v.is_null();
    let opt_str = |what: &str| -> Result<Option<String>, String> {
        if null {
            return Ok(None);
        }
        v.as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| format!("{what}: not a string"))
    };
    Ok(match key {
        "title" => {
            let t = v
                .as_str()
                .filter(|s| one_line(s))
                .ok_or("title: one non-empty line")?;
            Field::Title(t.to_string())
        }
        "type" => {
            let t = opt_str("type")?;
            if t.as_deref().is_some_and(|s| !one_line(s)) {
                return Err("type: one non-empty line".into());
            }
            Field::Kind(t)
        }
        "assignees" => {
            let a = str_list(v, "assignees")?;
            if a.is_empty() || a.iter().any(|s| s.is_empty()) {
                return Err("assignees: at least one seat".into());
            }
            if a.iter().collect::<BTreeSet<_>>().len() != a.len() {
                return Err("assignees: a seat twice".into());
            }
            Field::Assignees(a)
        }
        "size" => Field::Size(if null {
            None
        } else {
            Some(
                v.as_str()
                    .and_then(Size::parse)
                    .ok_or("size: XS, S, M, L, XL or XXL")?,
            )
        }),
        "blocked_by" => {
            let b = if null {
                Vec::new()
            } else {
                str_list(v, "blocked_by")?
            };
            if b.iter().collect::<BTreeSet<_>>().len() != b.len() {
                return Err("blocked_by: a task twice".into());
            }
            Field::BlockedBy(b)
        }
        "due" => Field::Due(if null { None } else { Some(date_of(v, "due")?) }),
        "after" => Field::After(if null {
            None
        } else {
            Some(date_of(v, "after")?)
        }),
        "when" => Field::When(if null { None } else { Some(When::parse(v)?) }),
        "repeat" => Field::Repeat(if null { None } else { Some(Repeat::parse(v)?) }),
        "skip" => Field::Skip(if null {
            Vec::new()
        } else {
            v.as_array()
                .ok_or("skip: not a list")?
                .iter()
                .map(|d| date_of(d, "skip"))
                .collect::<Result<_, _>>()?
        }),
        "moved" => {
            let mut out = BTreeMap::new();
            if !null {
                for (k, w) in v.as_object().ok_or("moved: not an object")? {
                    let d = parse_date(k).ok_or("moved: a key is not YYYY-MM-DD")?;
                    out.insert(d, When::parse(w)?);
                }
            }
            Field::Moved(out)
        }
        "description" => Field::Description(opt_str("description")?),
        "acceptance" => Field::Acceptance(if null {
            Vec::new()
        } else {
            line_list(v, "acceptance")?
        }),
        "out_of_scope" => Field::OutOfScope(if null {
            Vec::new()
        } else {
            line_list(v, "out_of_scope")?
        }),
        other => return Err(format!("unknown field `{other}`")),
    })
}

/// `after <= due` and "no self link" among the fields one act names.
fn check_fields(id: Option<&str>, fields: &[Field]) -> Result<(), String> {
    let mut due = None;
    let mut after = None;
    for f in fields {
        match f {
            Field::Due(d) => due = *d,
            Field::After(a) => after = *a,
            Field::BlockedBy(b) if id.is_some_and(|id| b.iter().any(|x| x == id)) => {
                return Err("blocked_by: a task cannot block itself".into())
            }
            _ => {}
        }
    }
    if let (Some(a), Some(d)) = (after, due) {
        if a > d {
            return Err("after is later than due".into());
        }
    }
    Ok(())
}

/// Whether ids may be `@ref` placeholders (the propose door, before
/// canonicalization) or must be canonical (the wire door, the fold).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Door {
    Propose,
    Canonical,
}

fn parse_act(i: usize, v: &Value, door: Door, refs: &BTreeSet<String>) -> Result<Act, String> {
    let at = |m: String| format!("ops[{i}]: {m}");
    let obj = v.as_object().ok_or_else(|| at("not an object".into()))?;
    let check_id = |s: &str| -> Result<(), String> {
        if let Some(r) = s.strip_prefix('@') {
            if door == Door::Canonical {
                return Err(at(format!("`{s}` is not a task id")));
            }
            if !refs.contains(r) {
                return Err(at(format!("unresolved `{s}`")));
            }
            return Ok(());
        }
        if is_hex32(s) {
            Ok(())
        } else {
            Err(at(format!("`{s}` is not a task id")))
        }
    };
    let id_of = |key: &str| -> Result<String, String> {
        let s = obj
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| at(format!("{key} missing")))?;
        check_id(s)?;
        Ok(s.to_string())
    };
    let check_links = |fields: &[Field]| -> Result<(), String> {
        for f in fields {
            if let Field::BlockedBy(b) = f {
                for x in b {
                    check_id(x)?;
                }
            }
        }
        Ok(())
    };
    match obj.get("act").and_then(Value::as_str) {
        Some("add") => {
            let mut fields = Vec::new();
            for (k, val) in obj {
                if matches!(k.as_str(), "act" | "id" | "ref" | "creator") {
                    continue;
                }
                if val.is_null() {
                    return Err(at(format!("add: {k} is null")));
                }
                fields.push(parse_field(k, val).map_err(at)?);
            }
            if !fields.iter().any(|f| matches!(f, Field::Title(_))) {
                return Err(at("add: title missing".into()));
            }
            if !fields.iter().any(|f| matches!(f, Field::Assignees(_))) {
                return Err(at("add: assignees missing".into()));
            }
            let has_when = fields.iter().any(|f| matches!(f, Field::When(Some(_))));
            if !has_when && fields.iter().any(|f| matches!(f, Field::Repeat(Some(_)))) {
                return Err(at("add: repeat needs when".into()));
            }
            check_links(&fields)?;
            let id = match obj.get("id") {
                None if door == Door::Canonical => return Err(at("add: id missing".into())),
                None => None,
                Some(_) => {
                    let s = id_of("id")?;
                    if s.starts_with('@') {
                        return Err(at("add: id is a task id".into()));
                    }
                    Some(s)
                }
            };
            let rf = match obj.get("ref") {
                None => None,
                Some(_) if door == Door::Canonical => {
                    return Err(at("add: ref is resolved at propose".into()))
                }
                Some(r) => Some(
                    r.as_str()
                        .filter(|r| is_ref_name(r))
                        .map(|r| format!("@{r}"))
                        .ok_or_else(|| at("add: ref is [a-z0-9_-]+".into()))?,
                ),
            };
            // the propose door overwrites whatever was supplied
            let creator = match (door, obj.get("creator")) {
                (Door::Propose, _) => None,
                (Door::Canonical, c) => Some(
                    c.and_then(Value::as_str)
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| at("add: creator missing".into()))?
                        .to_string(),
                ),
            };
            if door == Door::Canonical {
                if let Some(Field::Kind(Some(k))) =
                    fields.iter().find(|f| matches!(f, Field::Kind(_)))
                {
                    if k.trim() != k {
                        return Err(at("add: type is not trimmed".into()));
                    }
                }
            }
            for self_id in id.iter().chain(rf.iter()) {
                check_fields(Some(self_id), &fields).map_err(at)?;
            }
            check_fields(None, &fields).map_err(at)?;
            Ok(Act::Add {
                id,
                creator,
                fields,
            })
        }
        Some("set") => {
            for k in obj.keys() {
                if !matches!(k.as_str(), "act" | "id" | "fields") {
                    return Err(at(format!("set: unknown field `{k}`")));
                }
            }
            let id = id_of("id")?;
            let fobj = obj
                .get("fields")
                .and_then(Value::as_object)
                .filter(|f| !f.is_empty())
                .ok_or_else(|| at("set: fields names nothing".into()))?;
            let mut fields = Vec::new();
            for (k, val) in fobj {
                if matches!(k.as_str(), "state" | "note" | "evidence" | "creator") {
                    return Err(at(format!("set: {k} is not settable")));
                }
                if !TASK_FIELDS.contains(&k.as_str()) {
                    return Err(at(format!("set: unknown field `{k}`")));
                }
                if val.is_null() && matches!(k.as_str(), "title" | "assignees") {
                    return Err(at(format!("set: {k} cannot be cleared")));
                }
                let f = parse_field(k, val).map_err(at)?;
                if door == Door::Canonical {
                    if let Field::Kind(Some(t)) = &f {
                        if t.trim() != t {
                            return Err(at("set: type is not trimmed".into()));
                        }
                    }
                }
                fields.push(f);
            }
            check_links(&fields)?;
            check_fields(Some(&id), &fields).map_err(at)?;
            Ok(Act::Set { id, fields })
        }
        Some("state") => {
            for k in obj.keys() {
                if !matches!(k.as_str(), "act" | "id" | "to" | "note" | "evidence") {
                    return Err(at(format!("state: unknown field `{k}`")));
                }
            }
            let id = id_of("id")?;
            let to = obj
                .get("to")
                .and_then(Value::as_str)
                .and_then(TaskState::parse)
                .ok_or_else(|| at("state: unknown state".into()))?;
            let note = match obj.get("note") {
                None => None,
                Some(n) => Some(
                    n.as_str()
                        .ok_or_else(|| at("state: note is a string".into()))?
                        .to_string(),
                ),
            };
            let evidence = match obj.get("evidence") {
                None => Vec::new(),
                Some(_) if to != TaskState::Success => {
                    return Err(at("state: evidence only with success".into()))
                }
                Some(e) => str_list(e, "evidence").map_err(at)?,
            };
            Ok(Act::State {
                id,
                to,
                note,
                evidence,
            })
        }
        Some(other) => Err(at(format!("unknown act `{other}`"))),
        None => Err(at("act missing".into())),
    }
}

fn parse_changeset(v: &Value, door: Door) -> Result<Changeset, String> {
    let obj = v.as_object().ok_or("not an object")?;
    if obj.get("op").and_then(Value::as_str) != Some("kanban_ops") {
        return Err("op is kanban_ops".into());
    }
    for k in obj.keys() {
        if !matches!(k.as_str(), "op" | "summary" | "base_rev" | "ops") {
            return Err(format!("unknown field `{k}`"));
        }
    }
    if !obj
        .get("summary")
        .and_then(Value::as_str)
        .is_some_and(one_line)
    {
        return Err("summary: one non-empty line".into());
    }
    if obj.get("base_rev").and_then(Value::as_u64).is_none() {
        return Err("base_rev missing".into());
    }
    let ops = obj
        .get("ops")
        .and_then(Value::as_array)
        .filter(|o| !o.is_empty())
        .ok_or("ops: at least one act")?;
    let mut refs = BTreeSet::new();
    if door == Door::Propose {
        for op in ops {
            if op.get("act").and_then(Value::as_str) != Some("add") {
                continue;
            }
            if let Some(r) = op.get("ref").and_then(Value::as_str) {
                if !refs.insert(r.to_string()) {
                    return Err(format!("ref `{r}` twice"));
                }
            }
        }
    }
    let acts = ops
        .iter()
        .enumerate()
        .map(|(i, op)| parse_act(i, op, door, &refs))
        .collect::<Result<_, _>>()?;
    Ok(Changeset { acts })
}

/// The stateless shape check (§4.3) at the propose door, before
/// canonicalization: `@ref` placeholders, an `add` without `id` and a
/// supplied `creator` are allowed here.
///
/// # Errors
/// The first reason, one line.
pub fn validate_kanban_payload(v: &Value) -> Result<(), String> {
    parse_changeset(v, Door::Propose).map(|_| ())
}

/// The shape check at the wire door (§4.3): the payload must be
/// canonical - no `ref` key, no `@` value, every `add` with `id` and
/// `creator`, `type` trimmed.
///
/// # Errors
/// The first reason, one line.
pub fn validate_kanban_wire(v: &Value) -> Result<(), String> {
    parse_changeset(v, Door::Canonical).map(|_| ())
}

/// A payload after canonicalization at the propose door (§4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canonical {
    /// The payload members sign: ids only, every `add` with its `creator`.
    pub payload: Value,
    /// The ids the engine minted, in act order.
    pub minted: Vec<TaskId>,
}

/// Canonicalize a `kanban_ops` payload at the propose door (§4.2): mint
/// every missing `add` id, stamp `creator` (overwriting a supplied one),
/// trim `type`, replace every `@ref` and drop the `ref` keys.
///
/// # Errors
/// The shape check's first reason, or the minter's.
pub fn kanban_canonicalize(
    payload: &Value,
    creator: &str,
    mint: &mut dyn FnMut() -> Result<TaskId, String>,
) -> Result<Canonical, String> {
    validate_kanban_payload(payload)?;
    let mut out = payload.clone();
    let ops = out
        .get_mut("ops")
        .and_then(Value::as_array_mut)
        .ok_or("ops: at least one act")?;
    let mut refs: BTreeMap<String, String> = BTreeMap::new();
    let mut minted = Vec::new();
    for op in ops.iter_mut() {
        let Some(obj) = op.as_object_mut() else { continue };
        if obj.get("act").and_then(Value::as_str) != Some("add") {
            continue;
        }
        let id = match obj.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => {
                let id = mint()?;
                minted.push(id.clone());
                obj.insert("id".into(), Value::from(id.clone()));
                id
            }
        };
        if let Some(Value::String(r)) = obj.remove("ref") {
            refs.insert(format!("@{r}"), id);
        }
        obj.insert("creator".into(), Value::from(creator));
    }
    let resolve = |v: &mut Value| {
        if let Some(id) = v.as_str().and_then(|s| refs.get(s)) {
            *v = Value::from(id.clone());
        }
    };
    let trim = |obj: &mut Map<String, Value>| {
        if let Some(Value::String(t)) = obj.get_mut("type") {
            *t = t.trim().to_string();
        }
    };
    let links = |obj: &mut Map<String, Value>| {
        if let Some(list) = obj.get_mut("blocked_by").and_then(Value::as_array_mut) {
            list.iter_mut().for_each(resolve);
        }
    };
    for op in ops.iter_mut() {
        let Some(obj) = op.as_object_mut() else { continue };
        if let Some(id) = obj.get_mut("id") {
            resolve(id);
        }
        trim(obj);
        links(obj);
        if let Some(fields) = obj.get_mut("fields").and_then(Value::as_object_mut) {
            trim(fields);
            links(fields);
        }
    }
    validate_kanban_wire(&out)?;
    Ok(Canonical {
        payload: out,
        minted,
    })
}

// ----------------------------------------------------------------- fold --

/// What one applied payload did to the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FoldStep {
    /// The changeset applied.
    Applied,
    /// The changeset was void: `rev` moved, nothing else did.
    Void(VoidReason),
    /// Not a kanban changeset: the board is untouched, `rev` too.
    Skipped,
}

fn set_field(t: &mut Task, f: Field) {
    match f {
        Field::Title(v) => t.title = v,
        Field::Kind(v) => t.kind = v,
        Field::Assignees(v) => t.assignees = v,
        Field::Size(v) => t.size = v,
        Field::BlockedBy(v) => t.blocked_by = v,
        Field::Due(v) => t.due = v,
        Field::After(v) => t.after = v,
        Field::When(v) => t.when = v,
        Field::Repeat(v) => t.repeat = v,
        Field::Skip(v) => t.skip = v,
        Field::Moved(v) => t.moved = v,
        Field::Description(v) => t.description = v,
        Field::Acceptance(v) => t.acceptance = v,
        Field::OutOfScope(v) => t.out_of_scope = v,
    }
}

fn unmet_prerequisite(tasks: &BTreeMap<TaskId, Task>, t: &Task) -> Option<String> {
    t.blocked_by.iter().find_map(|b| match tasks.get(b) {
        Some(p) if p.state.is_met() => None,
        Some(p) => Some(format!("{} ({})", short_id(b), p.state.as_str())),
        None => Some(format!("{} (unknown)", short_id(b))),
    })
}

fn transition(
    tasks: &BTreeMap<TaskId, Task>,
    id: &str,
    to: TaskState,
    note: Option<&str>,
) -> Result<(), String> {
    use TaskState::{Cancelled, Fail, Success, Todo, Wip};
    let t = tasks
        .get(id)
        .ok_or_else(|| format!("{}: unknown task", short_id(id)))?;
    let from = t.state;
    let sid = short_id(id);
    let has_note = note.is_some_and(|n| !n.trim().is_empty());
    let needs_note = || {
        if has_note {
            Ok(())
        } else {
            Err(format!("{sid} {}: note required", to.as_str()))
        }
    };
    let guard = |verb: &str| match unmet_prerequisite(tasks, t) {
        None => Ok(()),
        Some(p) => Err(format!("{sid} cannot {verb}: blocked by {p}")),
    };
    if t.is_series() && !matches!((from, to), (Todo, Cancelled) | (Cancelled, Todo)) {
        return Err(format!("{sid}: a series only cancels or reopens"));
    }
    match (from, to) {
        (Todo, Wip) => guard("start"),
        (Wip, Todo) | (Wip, Success) | (Fail, Wip) => Ok(()),
        (Success | Fail | Cancelled, Todo) => Ok(()),
        (Wip, Fail) | (Todo | Wip | Fail, Cancelled) => needs_note(),
        (Todo, Success | Fail) => {
            if !t.is_timed_once() {
                return Err(format!("{sid} cannot {}: not started", to.as_str()));
            }
            guard(to.as_str())?;
            if to == Fail {
                needs_note()?;
            }
            Ok(())
        }
        _ => Err(format!(
            "{sid}: {} to {} not allowed",
            from.as_str(),
            to.as_str()
        )),
    }
}

/// The rules that need a task's resulting fields (§4.4), for every task
/// the changeset touched.
fn check_result(
    tasks: &BTreeMap<TaskId, Task>,
    touched: &BTreeSet<TaskId>,
    seats: &BTreeSet<String>,
) -> Result<(), String> {
    for id in touched {
        let Some(t) = tasks.get(id) else { continue };
        let sid = short_id(id);
        if !seats.contains(&t.creator) {
            return Err(format!("{sid}: creator {} is not a seat", t.creator));
        }
        if let Some(a) = t.assignees.iter().find(|a| !seats.contains(*a)) {
            return Err(format!("{sid}: {a} is not a seat"));
        }
        if let (Some(a), Some(d)) = (t.after, t.due) {
            if a > d {
                return Err(format!("{sid}: after is later than due"));
            }
        }
        if t.repeat.is_some() && t.when.is_none() {
            return Err(format!("{sid}: repeat needs when"));
        }
        if t.is_series() && !matches!(t.state, TaskState::Todo | TaskState::Cancelled) {
            return Err(format!("{sid}: a series is todo or cancelled"));
        }
        let keys = t.skip.iter().chain(t.moved.keys());
        match (&t.when, &t.repeat) {
            (Some(w), Some(r)) => {
                if let Some(k) = keys.into_iter().find(|k| !is_occurrence(w, r, **k)) {
                    return Err(format!("{sid}: {} is no occurrence", fmt_date(*k)));
                }
            }
            _ => {
                if !t.skip.is_empty() || !t.moved.is_empty() {
                    return Err(format!("{sid}: skip and moved need a series"));
                }
            }
        }
        for b in &t.blocked_by {
            if b == id {
                return Err(format!("{sid}: blocked by itself"));
            }
            let Some(p) = tasks.get(b) else {
                return Err(format!("{sid}: blocked by unknown {}", short_id(b)));
            };
            if p.is_series() || t.is_series() {
                return Err(format!("{sid}: a series cannot be linked"));
            }
        }
        if t.is_series() {
            if let Some((d, _)) = tasks.iter().find(|(_, d)| d.blocked_by.contains(id)) {
                return Err(format!("{}: a series cannot be linked", short_id(d)));
            }
        }
    }
    if let Some(id) = find_cycle(tasks, touched) {
        return Err(format!("{}: blocked_by forms a cycle", short_id(&id)));
    }
    Ok(())
}

/// A task on a `blocked_by` cycle reachable from `from`, if any
/// (iterative DFS - no recursion depth limit on a long chain).
fn find_cycle(tasks: &BTreeMap<TaskId, Task>, from: &BTreeSet<TaskId>) -> Option<TaskId> {
    // 1 = on the current path, 2 = done
    let mut color: BTreeMap<&str, u8> = BTreeMap::new();
    for root in from {
        if color.contains_key(root.as_str()) || !tasks.contains_key(root) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(root.as_str(), 0)];
        color.insert(root.as_str(), 1);
        while let Some((node, i)) = stack.pop() {
            let links = tasks.get(node).map_or(&[][..], |t| t.blocked_by.as_slice());
            if let Some(next) = links.get(i) {
                stack.push((node, i + 1));
                match color.get(next.as_str()) {
                    Some(1) => return Some(next.clone()),
                    Some(_) => {}
                    None if tasks.contains_key(next) => {
                        color.insert(next.as_str(), 1);
                        stack.push((next.as_str(), 0));
                    }
                    None => {}
                }
            } else {
                color.insert(node, 2);
            }
        }
    }
    None
}

fn apply_changeset(
    board: &BoardState,
    cs: Changeset,
    rev: u64,
    seats: &BTreeSet<String>,
) -> Result<BTreeMap<TaskId, Task>, String> {
    let mut tasks = board.tasks.clone();
    let mut touched = BTreeSet::new();
    for act in cs.acts {
        match act {
            Act::Add {
                id,
                creator,
                fields,
            } => {
                let id = id.ok_or("add: id missing")?;
                if tasks.contains_key(&id) {
                    return Err(format!("{}: already exists", short_id(&id)));
                }
                let mut t = Task {
                    title: String::new(),
                    kind: None,
                    creator: creator.unwrap_or_default(),
                    assignees: Vec::new(),
                    size: None,
                    blocked_by: Vec::new(),
                    due: None,
                    after: None,
                    when: None,
                    repeat: None,
                    skip: Vec::new(),
                    moved: BTreeMap::new(),
                    state: TaskState::Todo,
                    note: None,
                    evidence: Vec::new(),
                    description: None,
                    acceptance: Vec::new(),
                    out_of_scope: Vec::new(),
                    touched_rev: rev,
                };
                for f in fields {
                    set_field(&mut t, f);
                }
                tasks.insert(id.clone(), t);
                touched.insert(id);
            }
            Act::Set { id, fields } => {
                let t = tasks
                    .get_mut(&id)
                    .ok_or_else(|| format!("{}: unknown task", short_id(&id)))?;
                for f in fields {
                    set_field(t, f);
                }
                t.touched_rev = rev;
                touched.insert(id);
            }
            Act::State {
                id,
                to,
                note,
                evidence,
            } => {
                transition(&tasks, &id, to, note.as_deref())?;
                let t = tasks
                    .get_mut(&id)
                    .ok_or_else(|| format!("{}: unknown task", short_id(&id)))?;
                t.state = to;
                t.note = note;
                t.evidence = evidence;
                t.touched_rev = rev;
                touched.insert(id);
            }
        }
    }
    check_result(&tasks, &touched, seats)?;
    Ok(tasks)
}

/// Fold one applied payload onto `board`. A `kanban_ops` changeset either
/// applies whole or is void; anything else is skipped.
pub fn kanban_fold_one(
    board: &mut BoardState,
    payload: &Value,
    seats: &BTreeSet<String>,
) -> FoldStep {
    if payload.get("op").and_then(Value::as_str) != Some("kanban_ops") {
        return FoldStep::Skipped;
    }
    let rev = board.rev.saturating_add(1);
    let result = parse_changeset(payload, Door::Canonical)
        .and_then(|cs| apply_changeset(board, cs, rev, seats));
    board.rev = rev;
    match result {
        Ok(tasks) => {
            board.tasks = tasks;
            FoldStep::Applied
        }
        Err(e) => FoldStep::Void(VoidReason(e)),
    }
}

/// Fold more applied payloads onto a board (a cached prefix).
pub fn kanban_fold_onto(board: &mut BoardState, applied: &[Value], seats: &BTreeSet<String>) {
    for p in applied {
        kanban_fold_one(board, p, seats);
    }
}

/// THE board: every applied payload in chain order over the empty board.
#[must_use]
pub fn kanban_fold(applied: &[Value], seats: &BTreeSet<String>) -> BoardState {
    let mut board = BoardState::default();
    kanban_fold_onto(&mut board, applied, seats);
    board
}

/// The precheck at propose (§4.4): the fold's own apply over the current
/// board, for a canonical payload. `Err` carries the first reason the
/// changeset would void now.
///
/// # Errors
/// The first void reason.
pub fn kanban_precheck(
    board: &BoardState,
    payload: &Value,
    seats: &BTreeSet<String>,
) -> Result<(), VoidReason> {
    let cs = parse_changeset(payload, Door::Canonical).map_err(VoidReason)?;
    apply_changeset(board, cs, board.rev.saturating_add(1), seats)
        .map(|_| ())
        .map_err(VoidReason)
}

#[cfg(test)]
pub(crate) mod tests;
