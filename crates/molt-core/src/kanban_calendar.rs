//! The kanban calendar (`docs/kanban/kanban_workflows.md` §3): a task's
//! time window, the small RRULE subset of a recurring series, and the
//! expansion of a series into occurrences for a finite window.
//!
//! UTC only, no time zone, no clock: the caller passes every date. Dates
//! are accepted only in their canonical spelling (parse, format, compare),
//! so one value has one byte form on every node.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Days, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Weekday};
use serde_json::{Map, Value};

const DATE_FMT: &str = "%Y-%m-%d";
const DATETIME_FMT: &str = "%Y-%m-%dT%H:%M";
/// Four-digit years only: chrono's `%Y` round-trips signed years too.
const YEARS: std::ops::RangeInclusive<i32> = 0..=9999;

/// A `YYYY-MM-DD` date in its canonical spelling, or `None` (`2026-8-1`
/// does not round-trip and is refused).
#[must_use]
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let d = NaiveDate::parse_from_str(s, DATE_FMT).ok()?;
    (YEARS.contains(&d.year()) && fmt_date(d) == s).then_some(d)
}

/// A `YYYY-MM-DDTHH:MM` time in its canonical spelling, or `None`.
#[must_use]
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let t = NaiveDateTime::parse_from_str(s, DATETIME_FMT).ok()?;
    (YEARS.contains(&t.year()) && fmt_datetime(t) == s).then_some(t)
}

/// The canonical spelling of a date.
#[must_use]
pub fn fmt_date(d: NaiveDate) -> String {
    d.format(DATE_FMT).to_string()
}

/// The canonical spelling of a time.
#[must_use]
pub fn fmt_datetime(t: NaiveDateTime) -> String {
    t.format(DATETIME_FMT).to_string()
}

fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_time(NaiveTime::MIN)
}

/// A task's placement in time (§3.1): whole days (both inclusive) or a
/// timed span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum When {
    /// `start <= end`, both days inclusive.
    AllDay {
        /// First day.
        start: NaiveDate,
        /// Last day, inclusive.
        end: NaiveDate,
    },
    /// `start < end`.
    Timed {
        /// Start, UTC.
        start: NaiveDateTime,
        /// End, UTC.
        end: NaiveDateTime,
    },
}

impl When {
    /// Parse `{"start": S, "end": E}` - both all-day or both timed, in
    /// order, canonical spelling, no other key.
    ///
    /// # Errors
    /// The one-line reason.
    pub fn parse(v: &Value) -> Result<When, String> {
        let obj = v.as_object().ok_or("when: not an object")?;
        if obj.len() != 2 {
            return Err("when: exactly start and end".into());
        }
        let s = obj
            .get("start")
            .and_then(Value::as_str)
            .ok_or("when: start missing")?;
        let e = obj
            .get("end")
            .and_then(Value::as_str)
            .ok_or("when: end missing")?;
        if let (Some(start), Some(end)) = (parse_date(s), parse_date(e)) {
            return if start <= end {
                Ok(When::AllDay { start, end })
            } else {
                Err("when: end before start".into())
            };
        }
        if let (Some(start), Some(end)) = (parse_datetime(s), parse_datetime(e)) {
            return if start < end {
                Ok(When::Timed { start, end })
            } else {
                Err("when: end not after start".into())
            };
        }
        Err("when: start and end must both be YYYY-MM-DD or both YYYY-MM-DDTHH:MM".into())
    }

    /// The canonical JSON form.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let (s, e) = match self {
            When::AllDay { start, end } => (fmt_date(*start), fmt_date(*end)),
            When::Timed { start, end } => (fmt_datetime(*start), fmt_datetime(*end)),
        };
        let mut m = Map::new();
        m.insert("end".into(), Value::String(e));
        m.insert("start".into(), Value::String(s));
        Value::Object(m)
    }

    /// The day it starts on.
    #[must_use]
    pub fn start_date(&self) -> NaiveDate {
        match self {
            When::AllDay { start, .. } => *start,
            When::Timed { start, .. } => start.date(),
        }
    }

    /// The instant it begins (an all-day window at 00:00).
    #[must_use]
    pub fn begins(&self) -> NaiveDateTime {
        match self {
            When::AllDay { start, .. } => midnight(*start),
            When::Timed { start, .. } => *start,
        }
    }

    /// The instant it ends, exclusive (an all-day window at 00:00 of the
    /// day after its last day).
    #[must_use]
    pub fn ends(&self) -> NaiveDateTime {
        match self {
            When::AllDay { end, .. } => end
                .checked_add_days(Days::new(1))
                .map_or(NaiveDateTime::MAX, midnight),
            When::Timed { end, .. } => *end,
        }
    }

    fn shifted(&self, days: i64) -> Option<When> {
        let delta = TimeDelta::try_days(days)?;
        Some(match self {
            When::AllDay { start, end } => When::AllDay {
                start: start.checked_add_signed(delta)?,
                end: end.checked_add_signed(delta)?,
            },
            When::Timed { start, end } => When::Timed {
                start: start.checked_add_signed(delta)?,
                end: end.checked_add_signed(delta)?,
            },
        })
    }

    fn overlaps(&self, from: NaiveDate, to: NaiveDate) -> bool {
        let lo = midnight(from);
        let hi = to
            .checked_add_days(Days::new(1))
            .map_or(NaiveDateTime::MAX, midnight);
        self.begins() < hi && self.ends() > lo
    }
}

/// How often a series repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Freq {
    /// Every `interval` days.
    Daily,
    /// Every `interval` weeks, on `byday`.
    Weekly,
    /// Every `interval` months, on the start's day of month.
    Monthly,
}

impl Freq {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Freq::Daily => "daily",
            Freq::Weekly => "weekly",
            Freq::Monthly => "monthly",
        }
    }
}

const WEEKDAYS: [(&str, Weekday); 7] = [
    ("mo", Weekday::Mon),
    ("tu", Weekday::Tue),
    ("we", Weekday::Wed),
    ("th", Weekday::Thu),
    ("fr", Weekday::Fri),
    ("sa", Weekday::Sat),
    ("su", Weekday::Sun),
];

fn weekday_str(w: Weekday) -> &'static str {
    WEEKDAYS
        .iter()
        .find(|(_, d)| *d == w)
        .map_or("mo", |(s, _)| s)
}

/// The RRULE subset of §3.2.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RepeatEnd {
    /// An open series.
    Open,
    /// Last possible occurrence date, inclusive.
    Until(NaiveDate),
    /// Number of occurrences, counted before `skip`.
    Count(u64),
}

/// A recurring series' rule (§3.2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Repeat {
    /// Daily, weekly or monthly.
    pub freq: Freq,
    /// Every n-th period, `>= 1`.
    pub interval: u64,
    /// Weekly only; empty = the start's weekday.
    pub byday: Vec<Weekday>,
    /// How the series ends.
    pub end: RepeatEnd,
}

impl Repeat {
    /// Parse the repeat object.
    ///
    /// # Errors
    /// The one-line reason.
    pub fn parse(v: &Value) -> Result<Repeat, String> {
        let obj = v.as_object().ok_or("repeat: not an object")?;
        for k in obj.keys() {
            if !matches!(
                k.as_str(),
                "freq" | "interval" | "byday" | "until" | "count"
            ) {
                return Err(format!("repeat: unknown field `{k}`"));
            }
        }
        let freq = match obj.get("freq").and_then(Value::as_str) {
            Some("daily") => Freq::Daily,
            Some("weekly") => Freq::Weekly,
            Some("monthly") => Freq::Monthly,
            _ => return Err("repeat: freq is daily, weekly or monthly".into()),
        };
        let interval = match obj.get("interval") {
            None => 1,
            Some(n) => n
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or("repeat: interval >= 1")?,
        };
        let mut byday = Vec::new();
        if let Some(days) = obj.get("byday") {
            if freq != Freq::Weekly {
                return Err("repeat: byday only with weekly".into());
            }
            let days = days
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or("repeat: byday is a non-empty list")?;
            for d in days {
                let s = d.as_str().unwrap_or("");
                let w = WEEKDAYS
                    .iter()
                    .find(|(n, _)| *n == s)
                    .map(|(_, w)| *w)
                    .ok_or_else(|| format!("repeat: byday `{s}` is not mo..su"))?;
                if byday.contains(&w) {
                    return Err(format!("repeat: byday `{s}` twice"));
                }
                byday.push(w);
            }
        }
        let end = match (obj.get("until"), obj.get("count")) {
            (Some(_), Some(_)) => return Err("repeat: until or count, not both".into()),
            (Some(u), None) => RepeatEnd::Until(
                u.as_str()
                    .and_then(parse_date)
                    .ok_or("repeat: until is YYYY-MM-DD")?,
            ),
            (None, Some(c)) => {
                RepeatEnd::Count(c.as_u64().filter(|c| *c >= 1).ok_or("repeat: count >= 1")?)
            }
            (None, None) => RepeatEnd::Open,
        };
        Ok(Repeat {
            freq,
            interval,
            byday,
            end,
        })
    }

    /// The canonical JSON form (`interval` always spelled out).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if !self.byday.is_empty() {
            m.insert(
                "byday".into(),
                Value::Array(
                    self.byday
                        .iter()
                        .map(|w| Value::from(weekday_str(*w)))
                        .collect(),
                ),
            );
        }
        if let RepeatEnd::Count(c) = self.end {
            m.insert("count".into(), Value::from(c));
        }
        m.insert("freq".into(), Value::from(self.freq.as_str()));
        m.insert("interval".into(), Value::from(self.interval));
        if let RepeatEnd::Until(u) = self.end {
            m.insert("until".into(), Value::from(fmt_date(u)));
        }
        Value::Object(m)
    }
}

/// One expanded occurrence of a timed task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// The original occurrence date - the key `skip` and `moved` use.
    pub date: NaiveDate,
    /// The window it occupies (moved or not).
    pub window: When,
    /// Whether `moved` gave it its window.
    pub moved: bool,
}

fn day_mask(start: NaiveDate, r: &Repeat) -> u8 {
    let bit = |w: Weekday| 1u8 << w.num_days_from_monday();
    if r.byday.is_empty() {
        bit(start.weekday())
    } else {
        r.byday.iter().fold(0, |m, w| m | bit(*w))
    }
}

/// The bits `lo..hi` of a weekday mask.
fn mask_between(mask: u8, lo: u32, hi: u32) -> u64 {
    let span = (lo..hi).fold(0u8, |m, o| m | (1u8 << o));
    u64::from((mask & span).count_ones())
}

fn month_number(d: NaiveDate) -> i64 {
    i64::from(d.year()) * 12 + i64::from(d.month0())
}

/// Months in one Gregorian cycle (400 years): month lengths repeat with it.
const CYCLE_MONTHS: u64 = 4800;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// How many of the first `k` periods of a monthly series have its day.
fn months_with_day(start: NaiveDate, interval: u64, k: u64) -> u64 {
    let day = start.day();
    if day <= 28 {
        return k;
    }
    let m0 = u64::try_from(month_number(start)).unwrap_or(0) % CYCLE_MONTHS;
    let step = interval % CYCLE_MONTHS;
    let period = CYCLE_MONTHS / gcd(step, CYCLE_MONTHS);
    let hits = |n: u64| -> u64 {
        let mut c = 0;
        for j in 0..n {
            let a = (m0 + j * step) % CYCLE_MONTHS;
            // a year congruent mod 400 has the same month lengths
            let year = i32::try_from(2000 + a / 12).unwrap_or(2000);
            let month = u32::try_from(a % 12 + 1).unwrap_or(1);
            if NaiveDate::from_ymd_opt(year, month, day).is_some() {
                c += 1;
            }
        }
        c
    };
    (k / period) * hits(period) + hits(k % period)
}

/// The 0-based position of `date` among the series' original dates when
/// the rule's pattern hits it, ignoring `until`/`count`. Arithmetic, so a
/// far date costs no walk. The start is always position 0 (RFC 5545 DTSTART).
fn position(start: NaiveDate, r: &Repeat, date: NaiveDate) -> Option<u64> {
    if date < start {
        return None;
    }
    if date == start {
        return Some(0);
    }
    let days = u64::try_from((date - start).num_days()).ok()?;
    match r.freq {
        Freq::Daily => (days % r.interval == 0).then(|| days / r.interval),
        Freq::Weekly => {
            let mask = day_mask(start, r);
            let so = start.weekday().num_days_from_monday();
            let dof = date.weekday().num_days_from_monday();
            if mask & (1u8 << dof) == 0 {
                return None;
            }
            let weeks = (days + u64::from(so) - u64::from(dof)) / 7;
            if weeks % r.interval != 0 {
                return None;
            }
            let q = weeks / r.interval;
            Some(if q == 0 {
                1 + mask_between(mask, so + 1, dof)
            } else {
                1 + mask_between(mask, so + 1, 7)
                    + (q - 1) * u64::from(mask.count_ones())
                    + mask_between(mask, 0, dof)
            })
        }
        Freq::Monthly => {
            if date.day() != start.day() {
                return None;
            }
            let months = u64::try_from(month_number(date) - month_number(start)).ok()?;
            (months % r.interval == 0)
                .then(|| months_with_day(start, r.interval, months / r.interval))
        }
    }
}

fn within_end(r: &Repeat, date: NaiveDate, pos: u64) -> bool {
    match r.end {
        RepeatEnd::Open => true,
        RepeatEnd::Until(u) => date <= u,
        RepeatEnd::Count(c) => pos < c,
    }
}

/// Whether `date` is an original occurrence date of the series that
/// starts at `when` and repeats by `repeat`.
#[must_use]
pub fn is_occurrence(when: &When, repeat: &Repeat, date: NaiveDate) -> bool {
    position(when.start_date(), repeat, date).is_some_and(|p| within_end(repeat, date, p))
}

/// Expand a timed task into the occurrences overlapping `[from, to]`
/// (whole days, inclusive), ordered by start. A once-timed task (no
/// `repeat`) yields its one window; `skip` and `moved` apply to a series.
#[must_use]
pub fn expand(
    when: &When,
    repeat: Option<&Repeat>,
    skip: &[NaiveDate],
    moved: &BTreeMap<NaiveDate, When>,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<Occurrence> {
    let Some(r) = repeat else {
        return if when.overlaps(from, to) {
            vec![Occurrence {
                date: when.start_date(),
                window: *when,
                moved: false,
            }]
        } else {
            Vec::new()
        };
    };
    let start = when.start_date();
    let skip: BTreeSet<NaiveDate> = skip.iter().copied().collect();
    let mut out = Vec::new();
    let mut place = |d: NaiveDate| {
        if skip.contains(&d) {
            return;
        }
        let (window, is_moved) = match moved.get(&d) {
            Some(w) => (*w, true),
            None => match when.shifted((d - start).num_days()) {
                Some(w) => (w, false),
                None => return,
            },
        };
        if window.overlaps(from, to) {
            out.push(Occurrence {
                date: d,
                window,
                moved: is_moved,
            });
        }
    };
    // an occurrence starting up to its own length before `from` still overlaps
    let reach = TimeDelta::try_days((when.ends() - when.begins()).num_days() + 1);
    let lo = reach
        .and_then(|t| from.checked_sub_signed(t))
        .unwrap_or(start)
        .max(start);
    let mut pos: Option<u64> = None;
    let mut d = lo;
    while d <= to {
        let p = match pos {
            Some(p) => position_hit(start, r, d).then_some(p + 1),
            None => position(start, r, d),
        };
        if let Some(p) = p {
            if !within_end(r, d, p) {
                break;
            }
            pos = Some(p);
            place(d);
        }
        match d.succ_opt() {
            Some(n) => d = n,
            None => break,
        }
    }
    // an occurrence may have been moved into the window from outside it
    for k in moved.keys().filter(|k| **k < lo || **k > to) {
        if is_occurrence(when, r, *k) {
            place(*k);
        }
    }
    out.sort_by_key(|o| (o.window.begins(), o.date));
    out
}

/// `position(..).is_some()` without counting.
fn position_hit(start: NaiveDate, r: &Repeat, date: NaiveDate) -> bool {
    match r.freq {
        Freq::Monthly => {
            date.day() == start.day()
                && u64::try_from(month_number(date) - month_number(start))
                    .is_ok_and(|m| m % r.interval == 0)
        }
        _ => position(start, r, date).is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Months;
    use serde_json::json;

    fn d(s: &str) -> NaiveDate {
        parse_date(s).expect("fixture date")
    }

    fn rep(v: Value) -> Repeat {
        Repeat::parse(&v).expect("fixture repeat")
    }

    fn when(v: Value) -> When {
        When::parse(&v).expect("fixture when")
    }

    fn dates(occ: &[Occurrence]) -> Vec<String> {
        occ.iter().map(|o| fmt_date(o.date)).collect()
    }

    /// The plain walk the arithmetic replaced - the oracle it must agree with.
    fn walk(start: NaiveDate, r: &Repeat, limit: NaiveDate) -> Vec<NaiveDate> {
        let mut out = Vec::new();
        let mut push = |d: NaiveDate| -> bool {
            if d > limit {
                return false;
            }
            match r.end {
                RepeatEnd::Until(u) if d > u => return false,
                RepeatEnd::Count(c) if u64::try_from(out.len()).expect("len") >= c => return false,
                _ => {}
            }
            out.push(d);
            true
        };
        match r.freq {
            Freq::Daily => {
                let mut d = start;
                while push(d) {
                    d = d
                        .checked_add_days(Days::new(r.interval))
                        .expect("fixture range");
                }
            }
            Freq::Weekly => {
                if !push(start) {
                    return out;
                }
                let days: BTreeSet<u32> = if r.byday.is_empty() {
                    BTreeSet::from([start.weekday().num_days_from_monday()])
                } else {
                    r.byday.iter().map(|w| w.num_days_from_monday()).collect()
                };
                let mut monday = start
                    .checked_sub_days(Days::new(u64::from(start.weekday().num_days_from_monday())))
                    .expect("fixture range");
                'weeks: loop {
                    for off in &days {
                        let d = monday
                            .checked_add_days(Days::new(u64::from(*off)))
                            .expect("range");
                        if d > start && !push(d) {
                            break 'weeks;
                        }
                    }
                    monday = monday
                        .checked_add_days(Days::new(r.interval * 7))
                        .expect("range");
                    if monday > limit {
                        break;
                    }
                }
            }
            Freq::Monthly => {
                let first = start.with_day(1).expect("first");
                for k in 0u32.. {
                    let m = u32::try_from(r.interval).expect("interval") * k;
                    let month = first.checked_add_months(Months::new(m)).expect("range");
                    if month > limit {
                        break;
                    }
                    if let Some(d) = month.with_day(start.day()) {
                        if !push(d) {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn arithmetic_occurrence_agrees_with_the_walk() {
        let starts = [
            "2026-10-07",
            "2024-01-29",
            "2026-01-31",
            "2026-03-30",
            "2028-02-29",
        ];
        let rules = [
            json!({"freq":"daily"}),
            json!({"freq":"daily","interval":3,"count":40}),
            json!({"freq":"daily","interval":2,"until":"2027-02-01"}),
            json!({"freq":"weekly"}),
            json!({"freq":"weekly","interval":2,"byday":["mo","we","su"]}),
            json!({"freq":"weekly","interval":3,"byday":["tu"],"count":17}),
            json!({"freq":"weekly","byday":["fr","mo"],"until":"2027-05-05"}),
            json!({"freq":"monthly"}),
            json!({"freq":"monthly","interval":5,"count":9}),
            json!({"freq":"monthly","interval":13}),
            json!({"freq":"monthly","interval":12,"count":3}),
            json!({"freq":"monthly","interval":2,"until":"2029-01-31"}),
        ];
        let limit = d("2032-12-31");
        for s in starts {
            let w = when(json!({"start": s, "end": s}));
            for rule in &rules {
                let r = rep(rule.clone());
                let hits: BTreeSet<NaiveDate> =
                    walk(w.start_date(), &r, limit).into_iter().collect();
                let mut day = d("2023-12-01");
                while day <= limit {
                    assert_eq!(
                        is_occurrence(&w, &r, day),
                        hits.contains(&day),
                        "{s} {rule} {day}"
                    );
                    day = day.succ_opt().expect("next day");
                }
                for (from, to) in [
                    ("2026-10-01", "2026-10-31"),
                    ("2028-02-01", "2028-03-31"),
                    ("2023-01-01", "2032-12-31"),
                ] {
                    let skip = [d("2026-10-14"), d("2028-03-30")];
                    let got: Vec<NaiveDate> =
                        expand(&w, Some(&r), &skip, &BTreeMap::new(), d(from), d(to))
                            .iter()
                            .map(|o| o.date)
                            .collect();
                    let want: Vec<NaiveDate> = hits
                        .iter()
                        .copied()
                        .filter(|x| !skip.contains(x) && *x >= d(from) && *x <= d(to))
                        .collect();
                    assert_eq!(got, want, "{s} {rule} {from}..{to}");
                }
            }
        }
    }

    #[test]
    fn signed_years_are_refused() {
        assert!(parse_date("0000-01-01").is_some());
        assert!(parse_date("9999-12-31").is_some());
        assert!(parse_date("+10000-01-01").is_none());
        assert!(parse_date("-0001-12-31").is_none());
        assert!(parse_date("-262143-01-01").is_none());
        assert!(parse_datetime("+10000-01-01T00:00").is_none());
        assert!(parse_datetime("-0001-01-01T00:00").is_none());
    }

    #[test]
    fn far_keys_cost_no_walk() {
        let w = when(json!({"start":"0000-01-01","end":"0000-01-01"}));
        let r = rep(json!({"freq":"daily"}));
        let mut moved = BTreeMap::new();
        let mut day = d("9990-01-01");
        for _ in 0..3000 {
            assert!(is_occurrence(&w, &r, day));
            moved.insert(day, w);
            day = day.succ_opt().expect("next day");
        }
        let r = rep(json!({"freq":"monthly","interval":7}));
        let w31 = when(json!({"start":"0000-01-31","end":"0000-01-31"}));
        for y in 9000..9999 {
            let k = NaiveDate::from_ymd_opt(y, 12, 31).expect("date");
            let _ = is_occurrence(&w31, &r, k);
        }
        let occ = expand(
            &w,
            Some(&rep(json!({"freq":"daily"}))),
            &[],
            &moved,
            d("0000-01-01"),
            d("0000-01-01"),
        );
        assert_eq!(occ.len(), 3001);
    }

    #[test]
    fn dates_round_trip_or_are_refused() {
        assert!(parse_date("2026-08-01").is_some());
        assert!(parse_date("2026-8-1").is_none());
        assert!(parse_date("2026-02-30").is_none());
        assert!(parse_date("2026-08-01 ").is_none());
        assert!(parse_datetime("2026-11-03T09:00").is_some());
        assert!(parse_datetime("2026-11-03T9:00").is_none());
        assert!(parse_datetime("2026-11-03T09:00:00").is_none());
        assert!(parse_datetime("2026-11-03 09:00").is_none());
    }

    #[test]
    fn when_shapes() {
        assert!(When::parse(&json!({"start":"2026-10-01","end":"2026-10-01"})).is_ok());
        assert!(When::parse(&json!({"start":"2026-10-02","end":"2026-10-01"})).is_err());
        assert!(
            When::parse(&json!({"start":"2026-10-01T09:00","end":"2026-10-01T09:00"})).is_err()
        );
        assert!(When::parse(&json!({"start":"2026-10-01","end":"2026-10-01T09:00"})).is_err());
        assert!(When::parse(&json!({"start":"2026-10-01","end":"2026-10-02","x":1})).is_err());
        let w = when(json!({"start":"2026-11-03T09:00","end":"2026-11-03T10:00"}));
        assert_eq!(
            w.to_json(),
            json!({"start":"2026-11-03T09:00","end":"2026-11-03T10:00"})
        );
    }

    #[test]
    fn repeat_shapes() {
        assert!(Repeat::parse(&json!({"freq":"yearly"})).is_err());
        assert!(Repeat::parse(&json!({"freq":"daily","interval":0})).is_err());
        assert!(Repeat::parse(&json!({"freq":"daily","byday":["mo"]})).is_err());
        assert!(Repeat::parse(&json!({"freq":"weekly","byday":["xx"]})).is_err());
        assert!(Repeat::parse(&json!({"freq":"weekly","byday":["mo","mo"]})).is_err());
        assert!(Repeat::parse(&json!({"freq":"daily","until":"2027-01-01","count":3})).is_err());
        assert!(Repeat::parse(&json!({"freq":"daily","count":0})).is_err());
        assert!(Repeat::parse(&json!({"freq":"daily","until":"2027-1-1"})).is_err());
        let r = rep(json!({"freq":"weekly","byday":["mo","we"],"until":"2027-06-30"}));
        assert_eq!(
            r.to_json(),
            json!({"freq":"weekly","interval":1,"byday":["mo","we"],"until":"2027-06-30"})
        );
    }

    #[test]
    fn once_timed_is_its_one_window() {
        let w = when(json!({"start":"2026-10-12T09:00","end":"2026-10-12T10:00"}));
        let none = BTreeMap::new();
        assert_eq!(
            expand(&w, None, &[], &none, d("2026-10-01"), d("2026-10-31")).len(),
            1
        );
        assert!(expand(&w, None, &[], &none, d("2026-10-13"), d("2026-10-31")).is_empty());
    }

    #[test]
    fn weekly_defaults_to_the_start_weekday() {
        // 2026-10-05 is a Monday
        let w = when(json!({"start":"2026-10-05T09:00","end":"2026-10-05T09:30"}));
        let r = rep(json!({"freq":"weekly"}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-10-01"),
            d("2026-10-31"),
        );
        assert_eq!(
            dates(&occ),
            ["2026-10-05", "2026-10-12", "2026-10-19", "2026-10-26"]
        );
        assert_eq!(
            occ[1].window,
            when(json!({"start":"2026-10-12T09:00","end":"2026-10-12T09:30"}))
        );
    }

    #[test]
    fn weekly_byday_with_interval() {
        // start Wed 2026-10-07; every second week on mo and we
        let w = when(json!({"start":"2026-10-07","end":"2026-10-07"}));
        let r = rep(json!({"freq":"weekly","interval":2,"byday":["mo","we"]}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-10-01"),
            d("2026-11-04"),
        );
        assert_eq!(
            dates(&occ),
            [
                "2026-10-07",
                "2026-10-19",
                "2026-10-21",
                "2026-11-02",
                "2026-11-04"
            ]
        );
    }

    #[test]
    fn count_counts_before_skip_and_until_is_inclusive() {
        let w = when(json!({"start":"2026-10-01","end":"2026-10-01"}));
        let r = rep(json!({"freq":"daily","count":3}));
        let occ = expand(
            &w,
            Some(&r),
            &[d("2026-10-02")],
            &BTreeMap::new(),
            d("2026-09-01"),
            d("2026-12-31"),
        );
        assert_eq!(dates(&occ), ["2026-10-01", "2026-10-03"]);
        let r = rep(json!({"freq":"daily","interval":2,"until":"2026-10-05"}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-09-01"),
            d("2026-12-31"),
        );
        assert_eq!(dates(&occ), ["2026-10-01", "2026-10-03", "2026-10-05"]);
    }

    #[test]
    fn monthly_on_the_31st_skips_short_months() {
        let w = when(json!({"start":"2026-01-31T18:00","end":"2026-01-31T19:00"}));
        let r = rep(json!({"freq":"monthly"}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-01-01"),
            d("2026-12-31"),
        );
        assert_eq!(
            dates(&occ),
            [
                "2026-01-31",
                "2026-03-31",
                "2026-05-31",
                "2026-07-31",
                "2026-08-31",
                "2026-10-31",
                "2026-12-31"
            ]
        );
        assert!(!is_occurrence(&w, &r, d("2026-02-28")));
        assert!(is_occurrence(&w, &r, d("2026-08-31")));
        // count counts only the months that have the day
        let r = rep(json!({"freq":"monthly","count":2}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-01-01"),
            d("2026-12-31"),
        );
        assert_eq!(dates(&occ), ["2026-01-31", "2026-03-31"]);
    }

    #[test]
    fn skip_and_moved() {
        let w = when(json!({"start":"2026-10-05T09:00","end":"2026-10-05T10:00"}));
        let r = rep(json!({"freq":"weekly"}));
        let moved_to = when(json!({"start":"2026-10-13T14:00","end":"2026-10-13T15:00"}));
        let moved = BTreeMap::from([(d("2026-10-12"), moved_to)]);
        let occ = expand(
            &w,
            Some(&r),
            &[d("2026-10-19")],
            &moved,
            d("2026-10-01"),
            d("2026-10-31"),
        );
        assert_eq!(dates(&occ), ["2026-10-05", "2026-10-12", "2026-10-26"]);
        assert!(occ[1].moved);
        assert_eq!(occ[1].window, moved_to);
        // an occurrence moved INTO the window from beyond it
        let early = when(json!({"start":"2026-10-02T09:00","end":"2026-10-02T10:00"}));
        let moved = BTreeMap::from([(d("2026-11-02"), early)]);
        let occ = expand(&w, Some(&r), &[], &moved, d("2026-10-01"), d("2026-10-04"));
        assert_eq!(dates(&occ), ["2026-11-02"]);
        // and one moved OUT of it is gone from it
        let occ = expand(&w, Some(&r), &[], &moved, d("2026-11-01"), d("2026-11-03"));
        assert!(occ.is_empty());
    }

    #[test]
    fn a_multi_day_block_overlapping_the_window_start_is_kept() {
        let w = when(json!({"start":"2026-10-01","end":"2026-10-03"}));
        let r = rep(json!({"freq":"weekly"}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-10-10"),
            d("2026-10-10"),
        );
        assert_eq!(dates(&occ), ["2026-10-08"]);
        assert_eq!(
            occ[0].window,
            when(json!({"start":"2026-10-08","end":"2026-10-10"}))
        );
    }

    #[test]
    fn an_open_series_far_from_the_window_terminates() {
        let w = when(json!({"start":"2026-10-01","end":"2026-10-01"}));
        let r = rep(json!({"freq":"monthly","interval":1000000}));
        let occ = expand(
            &w,
            Some(&r),
            &[],
            &BTreeMap::new(),
            d("2026-01-01"),
            d("2030-12-31"),
        );
        assert_eq!(dates(&occ), ["2026-10-01"]);
    }
}
