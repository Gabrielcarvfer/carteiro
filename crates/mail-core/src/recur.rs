//! Expands repeating events (RFC 5545 RRULE) where no server does it for us:
//! calendar feeds and invitations received by mail. Covers what calendars
//! actually send: DAILY, WEEKLY, MONTHLY and YEARLY with INTERVAL, COUNT,
//! UNTIL, BYDAY (with positions such as 2MO or -1FR), BYMONTHDAY and
//! BYMONTH, plus EXDATE. Times repeat on the wall clock of the event, so a
//! 09:00 meeting stays at 09:00 across daylight-saving changes.

use crate::ical::{civil_from_days, days_from_civil, parse_datetime, Event};

#[derive(Debug, Default)]
struct Rule {
    freq: String,
    interval: i64,
    count: Option<usize>,
    until: Option<i64>,
    by_day: Vec<(i64, i64)>, // (position, 0 = Monday … 6 = Sunday); position 0 = every
    by_month_day: Vec<i64>,
    by_month: Vec<u32>,
}

fn weekday(day: i64) -> i64 {
    (day + 3).rem_euclid(7) // 1970-01-01 was a Thursday
}

fn parse_rule(s: &str) -> Option<Rule> {
    let mut r = Rule { interval: 1, ..Default::default() };
    for part in s.split(';') {
        let (k, v) = part.split_once('=')?;
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => r.freq = v.trim().to_ascii_uppercase(),
            "INTERVAL" => r.interval = v.trim().parse().ok().filter(|n| (1..=1000).contains(n))?,
            "COUNT" => r.count = Some(v.trim().parse().ok().filter(|n: &usize| *n > 0)?),
            "UNTIL" => r.until = parse_datetime(v).map(|x| x.0),
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.trim().to_ascii_uppercase();
                    if d.len() < 2 || !d.is_ascii() {
                        return None;
                    }
                    let (pos, name) = d.split_at(d.len() - 2);
                    let wd = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"].iter().position(|n| *n == name)? as i64;
                    let pos = if pos.is_empty() { 0 } else { pos.trim_start_matches('+').parse().ok().filter(|p: &i64| (-53..=53).contains(p) && *p != 0)? };
                    r.by_day.push((pos, wd));
                }
            }
            "BYMONTHDAY" => {
                for d in v.split(',') {
                    r.by_month_day.push(d.trim().parse().ok().filter(|n: &i64| (-31..=31).contains(n) && *n != 0)?);
                }
            }
            "BYMONTH" => {
                for m in v.split(',') {
                    r.by_month.push(m.trim().parse().ok().filter(|n| (1..=12).contains(n))?);
                }
            }
            _ => {} // WKST, BYSETPOS…: rarely used by invitations
        }
    }
    matches!(r.freq.as_str(), "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY").then_some(r)
}

fn month_len(y: i64, m: u32) -> i64 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)
}

/// Days of month (y, m) the rule picks; `start_day` supplies the defaults.
fn month_days(r: &Rule, y: i64, m: u32, start_day: i64) -> Vec<i64> {
    let first = days_from_civil(y, m, 1);
    let len = month_len(y, m);
    let mut out = Vec::new();
    if !r.by_month_day.is_empty() {
        for &d in &r.by_month_day {
            let d = if d > 0 { d } else { len + d + 1 };
            if (1..=len).contains(&d) {
                out.push(first + d - 1);
            }
        }
    } else if !r.by_day.is_empty() {
        for &(pos, wd) in &r.by_day {
            let all: Vec<i64> = (0..len).map(|i| first + i).filter(|&d| weekday(d) == wd).collect();
            match pos {
                0 => out.extend(all),
                p if p > 0 => out.extend(all.get(p as usize - 1)),
                p => out.extend(all.len().checked_sub((-p) as usize).and_then(|i| all.get(i))),
            }
        }
    } else {
        let (_, _, d) = civil_from_days(start_day);
        if i64::from(d) <= len {
            out.push(first + i64::from(d) - 1);
        }
    }
    out
}

/// Instances of `e` overlapping [from, to), at most `limit`. An event that
/// doesn't repeat (or whose rule can't be read) is returned as it is.
pub fn expand(e: &Event, from: i64, to: i64, limit: usize) -> Vec<Event> {
    let single = || if e.end > from && e.start < to { vec![e.clone()] } else { vec![] };
    let Some(r) = (!e.rrule.is_empty()).then(|| parse_rule(&e.rrule)).flatten() else { return single() };
    let d0 = e.start.div_euclid(86400);
    let tod = e.start.rem_euclid(86400);
    let dur = e.end - e.start;
    let (y0, m0, _) = civil_from_days(d0);
    let excluded = |t: i64| e.exdates.iter().any(|&x| x == t || (x.div_euclid(86400) == t.div_euclid(86400)));
    let mut out = Vec::new();
    let mut produced = 0usize;
    // Periods are bounded so a rule matching nothing can't spin forever.
    for k in 0..5000i64 {
        let mut days: Vec<i64> = match r.freq.as_str() {
            "DAILY" => vec![d0 + k * r.interval],
            "WEEKLY" => {
                let week = d0 - weekday(d0) + 7 * k * r.interval;
                let wds: Vec<i64> = if r.by_day.is_empty() { vec![weekday(d0)] } else { r.by_day.iter().map(|&(_, w)| w).collect() };
                wds.into_iter().map(|w| week + w).collect()
            }
            "MONTHLY" => {
                let idx = (y0 * 12 + i64::from(m0) - 1) + k * r.interval;
                month_days(&r, idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32, d0)
            }
            _ => {
                let y = y0 + k * r.interval;
                let months = if r.by_month.is_empty() { vec![m0] } else { r.by_month.clone() };
                months.into_iter().flat_map(|m| month_days(&r, y, m, d0)).collect()
            }
        };
        days.sort_unstable();
        days.dedup();
        for day in days {
            if r.freq == "DAILY" && !r.by_day.is_empty() && !r.by_day.iter().any(|&(_, w)| w == weekday(day)) {
                continue;
            }
            if !r.by_month.is_empty() && r.freq != "YEARLY" {
                let (_, m, _) = civil_from_days(day);
                if !r.by_month.contains(&m) {
                    continue;
                }
            }
            if r.freq == "DAILY" && !r.by_month_day.is_empty() {
                let (_, _, md) = civil_from_days(day);
                if !r.by_month_day.contains(&i64::from(md)) {
                    continue;
                }
            }
            let start = day * 86400 + tod;
            if start < e.start {
                continue;
            }
            if r.until.is_some_and(|u| start > u) || start >= to || r.count.is_some_and(|c| produced >= c) || out.len() >= limit {
                return out;
            }
            produced += 1;
            if excluded(start) || start + dur <= from {
                continue;
            }
            out.push(Event { start, end: start + dur, rrule: String::new(), exdates: Vec::new(), ..e.clone() });
        }
    }
    out
}

/// Expands every series in a calendar object into instances overlapping
/// [from, to): changed instances (RECURRENCE-ID) replace the ones they stand
/// for, and cancelled ones are dropped. At most `limit` per series.
pub fn expand_all(events: Vec<Event>, from: i64, to: i64, limit: usize) -> Vec<Event> {
    expand_all_with(events, from, to, limit, false)
}

/// `expand_all`, keeping cancelled series and dates (marked `cancelled`)
/// when `keep_cancelled`: invitations keep their history.
pub fn expand_all_with(events: Vec<Event>, from: i64, to: i64, limit: usize, keep_cancelled: bool) -> Vec<Event> {
    let (overrides, masters): (Vec<Event>, Vec<Event>) = events.into_iter().partition(|e| e.recurrence_id.is_some());
    let replaced = |uid: &str, start: i64| {
        overrides.iter().any(|o| o.uid == uid && o.recurrence_id.is_some_and(|r| r == start || r.div_euclid(86400) == start.div_euclid(86400)))
    };
    let mut out: Vec<Event> = Vec::new();
    for m in &masters {
        if m.cancelled && !keep_cancelled {
            continue;
        }
        out.extend(expand(m, from, to, limit).into_iter().filter(|i| m.uid.is_empty() || m.rrule.is_empty() || !replaced(&m.uid, i.start)));
    }
    out.extend(overrides.iter().filter(|o| (keep_cancelled || !o.cancelled) && o.end > from && o.start < to).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: &str, rule: &str) -> Event {
        let s = parse_datetime(start).unwrap().0;
        Event { title: "x".into(), start: s, end: s + 3600, rrule: rule.into(), recurring: true, ..Default::default() }
    }
    fn days(v: &[Event]) -> Vec<String> {
        v.iter().map(|e| crate::ical::fmt_date(e.start)).collect()
    }
    const FAR: i64 = 4_000_000_000;

    #[test]
    fn weekly_on_days_with_count() {
        // Mon 2026-11-09, every Mon and Wed, 4 times.
        let e = ev("20261109T090000", "FREQ=WEEKLY;BYDAY=MO,WE;COUNT=4");
        assert_eq!(days(&expand(&e, 0, FAR, 100)), ["20261109", "20261111", "20261116", "20261118"]);
        assert_eq!(expand(&e, 0, FAR, 100)[0].start % 86400, 9 * 3600, "same time of day");
    }

    #[test]
    fn monthly_positions_until_and_exdates() {
        let mut e = ev("20261110T100000", "FREQ=MONTHLY;BYDAY=2TU;UNTIL=20270301T000000Z");
        e.exdates.push(parse_datetime("20261208T100000").unwrap().0);
        assert_eq!(days(&expand(&e, 0, FAR, 100)), ["20261110", "20270112", "20270209"], "second Tuesdays, December skipped");
        let last = ev("20261127T100000", "FREQ=MONTHLY;BYDAY=-1FR;COUNT=3");
        assert_eq!(days(&expand(&last, 0, FAR, 100)), ["20261127", "20261225", "20270129"]);
        let day31 = ev("20270131T100000", "FREQ=MONTHLY;COUNT=3");
        assert_eq!(days(&expand(&day31, 0, FAR, 100)), ["20270131", "20270331", "20270531"], "months without a 31st are skipped");
    }

    #[test]
    fn moved_and_cancelled_instances() {
        let mut m = ev("20261109T090000", "FREQ=DAILY;COUNT=3");
        m.uid = "s".into();
        let mut moved = ev("20261110T150000", "");
        moved.uid = "s".into();
        moved.title = "moved".into();
        moved.recurrence_id = parse_datetime("20261110T090000").map(|x| x.0);
        let mut gone = ev("20261111T090000", "");
        gone.uid = "s".into();
        gone.cancelled = true;
        gone.recurrence_id = parse_datetime("20261111T090000").map(|x| x.0);
        let all = expand_all(vec![m, moved, gone], 0, FAR, 100);
        let got: Vec<(String, String)> = all.iter().map(|e| (crate::ical::fmt_utc(e.start), e.title.clone())).collect();
        assert_eq!(got, [("20261109T090000Z".to_string(), "x".to_string()), ("20261110T150000Z".to_string(), "moved".to_string())]);
    }

    #[test]
    fn daily_yearly_and_window() {
        let d = ev("20261109T080000", "FREQ=DAILY;INTERVAL=2");
        let from = parse_datetime("20261115T000000").unwrap().0;
        let to = parse_datetime("20261120T000000").unwrap().0;
        assert_eq!(days(&expand(&d, from, to, 100)), ["20261115", "20261117", "20261119"]);
        let y = ev("20240229T120000", "FREQ=YEARLY;COUNT=2");
        assert_eq!(days(&expand(&y, 0, FAR, 100)), ["20240229", "20280229"], "leap day only in leap years");
        assert_eq!(expand(&d, 0, FAR, 7).len(), 7, "limit");
        assert_eq!(expand(&ev("20261109T080000", "FREQ=SECONDLY"), 0, FAR, 10).len(), 1, "unknown rule: just the first");
    }
}
