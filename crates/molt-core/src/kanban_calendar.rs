//! The kanban calendar (`docs/kanban/kanban_workflows.md` §3): a task's
//! time window, the small RRULE subset of a recurring series, and the
//! expansion of a series into occurrences for a finite window.
//!
//! UTC only, no time zone, no clock: the caller passes every date. Dates
//! are accepted only in their canonical spelling (parse, format, compare),
//! so one value has one byte form on every node.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Weekday};
use serde_json::{Map, Value};

const DATE_FMT: &str = "%Y-%m-%d";
const DATETIME_FMT: &str = "%Y-%m-%dT%H:%M";

/// A `YYYY-MM-DD` date in its canonical spelling, or `None` (`2026-8-1`
/// does not round-trip and is refused).
#[must_use]
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let d = NaiveDate::parse_from_str(s, DATE_FMT).ok()?;
    (fmt_date(d) == s).then_some(d)
}

/// A `YYYY-MM-DDTHH:MM` time in its canonical spelling, or `None`.
#[must_use]
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let t = NaiveDateTime::parse_from_str(s, DATETIME_FMT).ok()?;
    (fmt_datetime(t) == s).then_some(t)
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

/// Visit the series' original occurrence dates (before `skip`) in order,
/// up to and including `limit`. The start always counts as the first
/// occurrence (RFC 5545 DTSTART).
fn series_dates(start: NaiveDate, r: &Repeat, limit: NaiveDate, mut emit: impl FnMut(NaiveDate)) {
    let mut emitted = 0u64;
    let mut push = |d: NaiveDate| -> bool {
        if d > limit {
            return false;
        }
        match r.end {
            RepeatEnd::Until(u) if d > u => return false,
            RepeatEnd::Count(c) if emitted >= c => return false,
            _ => {}
        }
        emitted += 1;
        emit(d);
        true
    };
    match r.freq {
        Freq::Daily => {
            let mut d = start;
            while push(d) {
                let Some(n) = d.checked_add_days(Days::new(r.interval)) else {
                    return;
                };
                d = n;
            }
        }
        Freq::Weekly => {
            if !push(start) {
                return;
            }
            let days: BTreeSet<u32> = if r.byday.is_empty() {
                BTreeSet::from([start.weekday().num_days_from_monday()])
            } else {
                r.byday.iter().map(|w| w.num_days_from_monday()).collect()
            };
            let Some(mut monday) = start
                .checked_sub_days(Days::new(u64::from(start.weekday().num_days_from_monday())))
            else {
                return;
            };
            let Some(step) = r.interval.checked_mul(7) else {
                return;
            };
            loop {
                for off in &days {
                    let Some(d) = monday.checked_add_days(Days::new(u64::from(*off))) else {
                        return;
                    };
                    if d > start && !push(d) {
                        return;
                    }
                }
                match monday.checked_add_days(Days::new(step)) {
                    Some(n) if n <= limit => monday = n,
                    _ => return,
                }
            }
        }
        Freq::Monthly => {
            let day = start.day();
            let Some(first) = start.with_day(1) else {
                return;
            };
            let mut k = 0u64;
            loop {
                let Some(month) = k
                    .checked_mul(r.interval)
                    .and_then(|m| u32::try_from(m).ok())
                    .and_then(|m| first.checked_add_months(Months::new(m)))
                else {
                    return;
                };
                if month > limit {
                    return;
                }
                // a month without that day is skipped (RFC 5545)
                if let Some(d) = month.with_day(day) {
                    if !push(d) {
                        return;
                    }
                }
                k += 1;
            }
        }
    }
}

/// Whether `date` is an original occurrence date of the series that
/// starts at `when` and repeats by `repeat`.
#[must_use]
pub fn is_occurrence(when: &When, repeat: &Repeat, date: NaiveDate) -> bool {
    let mut hit = false;
    series_dates(when.start_date(), repeat, date, |d| hit |= d == date);
    hit
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
    series_dates(start, r, to, &mut place);
    // a later occurrence may have been moved into the window
    for d in moved.keys().filter(|d| **d > to) {
        if is_occurrence(when, r, *d) {
            place(*d);
        }
    }
    out.sort_by_key(|o| (o.window.begins(), o.date));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
