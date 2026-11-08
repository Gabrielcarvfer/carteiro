//! Outlook-style search queries: free words plus `from:`, `to:`, `subject:`,
//! `has:attachment`, `is:unread`, `is:flagged`, `after:` and `before:`, and
//! attachment types: `has:attachment:pdf`, or just `has:pdf`, `has:word`,
//! `has:excel`, `has:powerpoint`, `has:image`, `has:calendar`, `has:zip`,
//! or any file extension (`has:dwg`).
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
    /// Attachment types asked for: each entry is a set of file extensions
    /// (`has:word` is doc, docx, odt…), and every entry must match.
    pub attachment_types: Vec<Vec<String>>,
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

/// The file extensions a `has:` value stands for (`word` -> doc, docx, …);
/// None when it isn't a type.
pub fn type_extensions(v: &str) -> Option<Vec<String>> {
    let v = v.trim_start_matches('.').to_ascii_lowercase();
    let list: &[&str] = match v.as_str() {
        "pdf" => &["pdf"],
        "word" | "doc" | "docx" | "document" | "documents" => &["doc", "docx", "docm", "dotx", "odt", "rtf"],
        "excel" | "xls" | "xlsx" | "sheet" | "sheets" | "spreadsheet" => &["xls", "xlsx", "xlsm", "ods", "csv"],
        "powerpoint" | "ppt" | "pptx" | "slides" | "presentation" => &["ppt", "pptx", "pptm", "ppsx", "odp", "key"],
        "image" | "images" | "photo" | "photos" | "picture" | "pictures" => &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff", "svg"],
        "calendar" | "invite" | "invitation" | "ics" => &["ics", "ical", "vcs"],
        "zip" | "archive" | "archives" => &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz"],
        "video" | "videos" => &["mp4", "mov", "avi", "mkv", "webm", "m4v"],
        "audio" | "sound" => &["mp3", "m4a", "wav", "ogg", "opus", "flac", "aac"],
        "text" | "txt" => &["txt", "md", "log"],
        _ if !v.is_empty() && v.len() <= 8 && v.bytes().all(|b| b.is_ascii_alphanumeric()) && !matches!(v.as_str(), "attachment" | "attachments" | "files") => {
            return Some(vec![v]);
        }
        _ => return None,
    };
    Some(list.iter().map(|s| s.to_string()).collect())
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
            // has:attachment:pdf, has:pdf, has:word…
            ("has", v) if v.starts_with("attachment:") || v.starts_with("attachments:") || v.starts_with("file:") => {
                match v.split_once(':').and_then(|(_, t)| type_extensions(t)) {
                    Some(exts) => {
                        out.has_attachment = true;
                        out.attachment_types.push(exts);
                    }
                    None => out.words.push(t),
                }
            }
            ("has", v) if type_extensions(v).is_some() => {
                out.has_attachment = true;
                out.attachment_types.push(type_extensions(v).unwrap_or_default());
            }
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
    fn attachment_types() {
        let q = parse("has:attachment:pdf budget");
        assert!(q.has_attachment);
        assert_eq!(q.attachment_types, [vec!["pdf".to_string()]]);
        assert_eq!(q.words, ["budget"]);
        assert!(parse("has:word").attachment_types[0].contains(&"docx".to_string()));
        assert!(parse("HAS:Excel").attachment_types[0].contains(&"xlsx".to_string()));
        assert!(parse("has:attachment:.DWG").attachment_types[0] == ["dwg"]);
        assert_eq!(parse("has:pdf has:image").attachment_types.len(), 2, "both kinds required");
        assert!(parse("has:attachment").attachment_types.is_empty());
        assert_eq!(parse("has:attachment:what?ever").words, ["has:attachment:what?ever"]);
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
