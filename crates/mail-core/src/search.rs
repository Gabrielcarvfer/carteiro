//! Outlook-style search queries: free words plus `from:`, `to:`, `subject:`,
//! `has:attachment`, `is:unread`, `is:flagged`, `after:` and `before:`.
//! Quotes group words ("q1 roadmap"); unknown `key:value` pairs are searched
//! as plain words.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Each must match subject, people, preview or (clean) message text.
    pub words: Vec<String>,
    pub from: Vec<String>,
    /// To or Cc. "me" means the account's own address.
    pub to: Vec<String>,
    pub subject: Vec<String>,
    pub has_attachment: bool,
    pub unread: bool,
    pub flagged: bool,
    /// Unix time bounds (local midnight is close enough: UTC dates).
    pub after: Option<i64>,
    pub before: Option<i64>,
}

impl Query {
    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }
}

/// Splits on spaces, keeping quoted phrases (and `key:"a phrase"`) together.
fn tokens(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in q.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// "2026-10-01" -> Unix time at 00:00 UTC.
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400)
}

pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    for t in tokens(q) {
        let Some((key, value)) = t.split_once(':') else {
            out.words.push(t);
            continue;
        };
        let v = value.to_string();
        match (key.to_ascii_lowercase().as_str(), value.to_ascii_lowercase().as_str()) {
            ("from", _) if !v.is_empty() => out.from.push(v),
            ("to" | "cc", _) if !v.is_empty() => out.to.push(v),
            ("subject", _) if !v.is_empty() => out.subject.push(v),
            ("has", "attachment" | "attachments" | "files") => out.has_attachment = true,
            ("is", "unread") => out.unread = true,
            ("is", "flagged") => out.flagged = true,
            ("after" | "since", _) if parse_date(value).is_some() => out.after = parse_date(value),
            ("before", _) if parse_date(value).is_some() => out.before = parse_date(value),
            _ => out.words.push(t), // e.g. "10:30" or "https://..."
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_outlook_style_queries() {
        let q = parse(r#"from:bob to:me subject:"q1 roadmap" has:attachment is:unread budget "next week" after:2026-10-01 before:2026-11-01"#);
        assert_eq!(q.from, ["bob"]);
        assert_eq!(q.to, ["me"]);
        assert_eq!(q.subject, ["q1 roadmap"]);
        assert!(q.has_attachment && q.unread && !q.flagged);
        assert_eq!(q.words, ["budget", "next week"]);
        assert_eq!(q.after, Some(1_790_812_800)); // 2026-10-01T00:00:00Z
        assert_eq!(q.before, Some(1_793_491_200)); // 2026-11-01T00:00:00Z
    }

    #[test]
    fn unknown_or_malformed_keys_are_words() {
        let q = parse("meet 10:30 http://x.org from: after:yesterday");
        assert_eq!(q.words, ["meet", "10:30", "http://x.org", "from:", "after:yesterday"]);
        assert!(parse("   ").is_empty());
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-13-01"), None);
    }
}
