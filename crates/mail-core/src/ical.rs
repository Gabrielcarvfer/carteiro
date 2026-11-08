//! Just enough iCalendar (RFC 5545) for calendar sync: read VEVENTs that the
//! server has already expanded to UTC, and write simple events.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub uid: String,
    pub title: String,
    pub location: String,
    pub description: String,
    /// Unix seconds. All-day events start at 00:00 UTC of their date.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// Times had no zone (or one we couldn't resolve): show them as wall
    /// clock time rather than converting from UTC.
    pub floating: bool,
    pub recurring: bool,
}

// ---- dates (proleptic Gregorian, no external crate) --------------------------

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// "20261109" / "20261109T093000" / "20261109T093000Z" -> (unix secs, is_date, is_utc).
pub fn parse_datetime(v: &str) -> Option<(i64, bool, bool)> {
    let v = v.trim();
    if !v.is_ascii() {
        return None; // also keeps the byte slicing below on char boundaries
    }
    let date = |s: &str| Some(days_from_civil(num(s.get(0..4)?)?, num(s.get(4..6)?)? as u32, num(s.get(6..8)?)? as u32) * 86400);
    if v.len() == 8 {
        return Some((date(v)?, true, true));
    }
    let (body, utc) = match v.strip_suffix('Z') {
        Some(b) => (b, true),
        None => (v, false),
    };
    if body.len() != 15 || body.as_bytes()[8] != b'T' {
        return None;
    }
    let t = &body[9..];
    let secs = num(&t[0..2])? * 3600 + num(&t[2..4])? * 60 + num(&t[4..6])?;
    Some((date(body)? + secs, false, utc))
}

pub fn fmt_utc(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    let s = ts.rem_euclid(86400);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

pub fn fmt_date(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    format!("{y:04}{m:02}{d:02}")
}

/// ISO 8601 for Graph / CalDAV time-range ("2026-11-09T09:30:00").
pub fn fmt_iso(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    let s = ts.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// "2026-11-09T09:30:00.0000000" (Graph) -> unix seconds, read as UTC.
pub fn parse_iso(v: &str) -> Option<i64> {
    let v = v.trim().trim_end_matches('Z');
    if !v.is_ascii() {
        return None;
    }
    let (date, time) = v.split_once('T').unwrap_or((v, "00:00:00"));
    let mut d = date.split('-');
    let days = days_from_civil(num(d.next()?)?, num(d.next()?)? as u32, num(d.next()?)? as u32);
    let mut t = time.split(':');
    let h = num(t.next()?)?;
    let mi = num(t.next().unwrap_or("0"))?;
    let sec = num(t.next().unwrap_or("0").split('.').next().unwrap_or("0")).unwrap_or(0);
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// "PT1H30M", "P1D", "P2W" -> seconds.
pub fn parse_duration(v: &str) -> Option<i64> {
    let v = v.trim();
    let (neg, v) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let mut total = 0i64;
    let mut n = String::new();
    for c in v.chars() {
        match c {
            '0'..='9' => n.push(c),
            'T' => {}
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let k: i64 = n.parse().ok()?;
                n.clear();
                total += k * match c {
                    'W' => 604800,
                    'D' => 86400,
                    'H' => 3600,
                    'M' => 60,
                    _ => 1,
                };
            }
            _ => return None,
        }
    }
    Some(if neg { -total } else { total })
}

// ---- reading -----------------------------------------------------------------

fn unfold(ics: &str) -> String {
    ics.replace("\r\n ", "").replace("\r\n\t", "").replace("\n ", "").replace("\n\t", "")
}

pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// Splits "NAME;PARAM=x;VALUE=DATE:value" into (NAME, params, value).
fn split_prop(line: &str) -> Option<(String, String, &str)> {
    // The value starts at the first ':' outside a quoted parameter.
    let mut quoted = false;
    let idx = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?.0;
    let head = &line[..idx];
    let (name, params) = head.split_once(';').unwrap_or((head, ""));
    Some((name.to_ascii_uppercase(), params.to_ascii_uppercase(), &line[idx + 1..]))
}

/// Every VEVENT in a VCALENDAR object (including expanded instances).
pub fn parse_events(ics: &str) -> Vec<Event> {
    let mut out = Vec::new();
    let mut cur: Option<Event> = None;
    let mut end: Option<i64> = None;
    let mut duration: Option<i64> = None;
    let mut depth = 0; // nested components inside VEVENT (VALARM)
    for line in unfold(ics).lines() {
        let line = line.trim_end_matches('\r');
        let Some((name, params, value)) = split_prop(line) else { continue };
        match (name.as_str(), cur.is_some()) {
            ("BEGIN", false) if value.eq_ignore_ascii_case("VEVENT") => {
                cur = Some(Event::default());
                end = None;
                duration = None;
                depth = 0;
            }
            ("BEGIN", true) => depth += 1,
            ("END", true) if depth > 0 => depth -= 1,
            ("END", true) if value.eq_ignore_ascii_case("VEVENT") => {
                let mut e = cur.take().unwrap();
                e.end = match (end, duration) {
                    (Some(t), _) => t,
                    (None, Some(d)) => e.start + d,
                    (None, None) => e.start + if e.all_day { 86400 } else { 0 },
                };
                if e.end < e.start {
                    e.end = e.start;
                }
                if e.start != 0 || e.end != 0 {
                    out.push(e);
                }
            }
            (_, true) if depth > 0 => {}
            (prop, true) => {
                let e = cur.as_mut().unwrap();
                match prop {
                    "UID" => e.uid = value.trim().to_string(),
                    "SUMMARY" => e.title = unescape(value),
                    "LOCATION" => e.location = unescape(value),
                    "DESCRIPTION" => e.description = unescape(value),
                    "RRULE" | "RECURRENCE-ID" => e.recurring = true,
                    "DTSTART" => {
                        if let Some((t, is_date, utc)) = parse_datetime(value) {
                            e.start = t;
                            e.all_day = is_date || params.contains("VALUE=DATE") && !params.contains("VALUE=DATE-TIME");
                            e.floating = !utc;
                        }
                    }
                    "DTEND" => end = parse_datetime(value).map(|x| x.0),
                    "DURATION" => duration = parse_duration(value),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

// ---- writing -----------------------------------------------------------------

/// Folds lines longer than 75 octets (RFC 5545 3.1), on char boundaries.
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for c in line.chars() {
        if len + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(c);
        len += c.len_utf8();
    }
    out
}

pub fn build_event(e: &Event, now: i64) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//Carteiro//EN".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{}", e.uid),
        format!("DTSTAMP:{}", fmt_utc(now)),
    ];
    if e.all_day {
        lines.push(format!("DTSTART;VALUE=DATE:{}", fmt_date(e.start)));
        lines.push(format!("DTEND;VALUE=DATE:{}", fmt_date(e.end.max(e.start + 86400))));
    } else {
        lines.push(format!("DTSTART:{}", fmt_utc(e.start)));
        lines.push(format!("DTEND:{}", fmt_utc(e.end.max(e.start))));
    }
    lines.push(format!("SUMMARY:{}", escape(&e.title)));
    if !e.location.is_empty() {
        lines.push(format!("LOCATION:{}", escape(&e.location)));
    }
    if !e.description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape(&e.description)));
    }
    lines.push("END:VEVENT".into());
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| fold(l)).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_math_roundtrips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        for z in [-1000, 0, 11016, 20766, 30000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(parse_datetime("20261109T093000Z"), Some((1794216600, false, true)));
        assert_eq!(parse_datetime("20261109"), Some((1794182400, true, true)));
        assert_eq!(parse_datetime("20261109T093000").map(|x| x.2), Some(false));
        assert_eq!(fmt_utc(1794216600), "20261109T093000Z");
        assert_eq!(parse_iso("2026-11-09T09:30:00.0000000"), Some(1794216600));
        assert_eq!(fmt_iso(1794216600), "2026-11-09T09:30:00");
        assert_eq!(parse_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_duration("P1W"), Some(604800));
    }

    #[test]
    fn parses_expanded_events() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc\r\nSUMMARY:Team sync\\, weekly\r\nDESCRIPTION:Line 1\\nLine\r\n  2\r\nDTSTART:20261109T093000Z\r\nDURATION:PT30M\r\nRECURRENCE-ID:20261109T093000Z\r\nBEGIN:VALARM\r\nSUMMARY:not me\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:day\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261225\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ev = parse_events(ics);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].title, "Team sync, weekly");
        assert_eq!(ev[0].description, "Line 1\nLine 2");
        assert_eq!(ev[0].end - ev[0].start, 1800);
        assert!(ev[0].recurring && !ev[0].all_day && !ev[0].floating);
        assert!(ev[1].all_day);
        assert_eq!(ev[1].end - ev[1].start, 86400);
    }

    #[test]
    fn build_then_parse() {
        let e = Event {
            uid: "u1@carteiro".into(),
            title: "Lunch; with, Ana".into(),
            location: "Café".into(),
            description: "a\nb".into(),
            start: 1794216600,
            end: 1794220200,
            ..Default::default()
        };
        let back = &parse_events(&build_event(&e, 0))[0];
        assert_eq!(back, &e);
    }
}
