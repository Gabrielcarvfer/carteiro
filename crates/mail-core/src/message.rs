//! Message parsing (via mail-parser), composing (via mail-builder) and the
//! text helpers that make conversations readable.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addr {
    pub name: String,
    pub email: String,
}

impl Addr {
    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub message_id: String,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub date: i64,
    pub has_attachments: bool,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub name: String,
    pub mime: String,
    pub size: usize,
    /// Why this file type is dangerous (see `safety`), if it is.
    #[serde(default)]
    pub danger: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Body {
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
}

fn conv(a: &mail_parser::Addr) -> Addr {
    Addr {
        name: a.name.as_deref().unwrap_or("").trim().to_string(),
        email: a.address.as_deref().unwrap_or("").trim().to_lowercase(),
    }
}

fn addrs(a: Option<&Address>) -> Vec<Addr> {
    match a {
        Some(Address::List(list)) => list.iter().map(conv).collect(),
        Some(Address::Group(groups)) => groups.iter().flat_map(|g| g.addresses.iter().map(conv)).collect(),
        None => Vec::new(),
    }
}

fn ids(v: &HeaderValue) -> Vec<String> {
    let clean = |s: &str| s.trim().trim_start_matches('<').trim_end_matches('>').to_string();
    match v {
        HeaderValue::Text(t) => vec![clean(t)],
        HeaderValue::TextList(l) => l.iter().map(|s| clean(s)).collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect()
}

/// Parses headers (and the body, if present) into the fields we index.
/// Works with header-only fetches as well as full messages.
pub fn parse_meta(raw: &[u8]) -> MessageMeta {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return MessageMeta { message_id: synthetic_id(raw), ..Default::default() };
    };
    let from = addrs(msg.from()).into_iter().next().unwrap_or_default();
    let date = msg.date().map(|d| d.to_timestamp()).unwrap_or(0);
    let subject = msg.subject().unwrap_or("").trim().to_string();
    let mut message_id = msg.message_id().unwrap_or("").trim().to_string();
    if message_id.is_empty() {
        message_id = synthetic_id(format!("{}|{}|{}", from.email, date, subject).as_bytes());
    }
    let multipart_mixed = msg
        .content_type()
        .is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("multipart") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed")));
    let snippet = msg.body_text(0).map(|t| snippet_of(&t)).unwrap_or_default();
    MessageMeta {
        message_id,
        in_reply_to: ids(msg.in_reply_to()),
        references: ids(msg.references()),
        subject,
        from,
        to: addrs(msg.to()),
        cc: addrs(msg.cc()),
        date,
        has_attachments: msg.attachment_count() > 0 || multipart_mixed,
        snippet,
    }
}

/// An attachment's file name; unnamed parts get one from their type (an
/// invitation sent inline becomes "invitation.ics").
fn part_name(p: &mail_parser::MessagePart) -> String {
    if let Some(n) = p.attachment_name() {
        return n.to_string();
    }
    let calendar = p.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("text") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("calendar")));
    if calendar { "invitation.ics" } else { "attachment" }.to_string()
}

pub fn parse_body(raw: &[u8]) -> Body {
    let Some(msg) = MessageParser::default().parse(raw) else { return Body::default() };
    let attachments = msg
        .attachments()
        .map(|p| AttachmentInfo {
            danger: crate::safety::attachment_risk(p.attachment_name().unwrap_or("")),
            name: part_name(p),
            mime: p
                .content_type()
                .map(|c| match c.subtype() {
                    Some(s) => format!("{}/{}", c.ctype(), s),
                    None => c.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".into()),
            size: p.contents().len(),
        })
        .collect();
    Body {
        text: msg.body_text(0).map(|t| t.into_owned()),
        // Only a real text/html part: body_html() also converts plain text to
        // HTML, which would bypass plain-text handling (folded quotes).
        html: match msg.html_part(0).map(|p| &p.body) {
            Some(mail_parser::PartType::Html(h)) => Some(h.to_string()),
            _ => None,
        },
        attachments,
    }
}

/// The decoded contents of attachment `index` (as listed by `parse_body`).
pub fn attachment_bytes(raw: &[u8], index: usize) -> Option<(String, Vec<u8>)> {
    let msg = MessageParser::default().parse(raw)?;
    let part = msg.attachments().nth(index)?;
    Some((part_name(part), part.contents().to_vec()))
}

/// Every attachment's (name, contents), in `parse_body` order, from one
/// parse of the message (asking for each by index parses it each time).
pub fn all_attachments(raw: &[u8]) -> Vec<(String, Vec<u8>)> {
    let Some(msg) = MessageParser::default().parse(raw) else { return Vec::new() };
    msg.attachments().map(|p| (part_name(p), p.contents().to_vec())).collect()
}

fn synthetic_id(seed: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    format!("{:016x}@carteiro.local", h.finish())
}

/// First ~160 chars of the new (non-quoted) part, whitespace collapsed.
pub fn snippet_of(text: &str) -> String {
    let (new, _) = split_quoted(text);
    let collapsed = new.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(160).collect()
}

/// Splits a plain-text reply into (what the sender wrote, quoted history).
/// Recognizes `>` quoting, "On ... wrote:" (Gmail/Apple), and Outlook's
/// "-----Original Message-----" / "From: ... Sent:" blocks.
pub fn split_quoted(text: &str) -> (String, String) {
    let lines: Vec<&str> = text.lines().collect();
    // tail_quoted[i]: lines[i..] are all quoted or blank.
    let mut tail_quoted = vec![true; lines.len() + 1];
    for i in (0..lines.len()).rev() {
        let t = lines[i].trim();
        tail_quoted[i] = tail_quoted[i + 1] && (t.is_empty() || t.starts_with('>'));
    }
    let mut cut = lines.len();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        let next = lines.get(i + 1).map(|n| n.trim()).unwrap_or("");
        let header = t == "-----Original Message-----"
            || t.starts_with("________________________________")
            || (t.starts_with("On ") && t.ends_with("wrote:"))
            || (t.starts_with("On ") && !t.ends_with('.') && next.ends_with("wrote:") && next.len() < 120)
            || (t.starts_with("From:") && (next.starts_with("Sent:") || next.starts_with("Date:")));
        if header || (t.starts_with('>') && tail_quoted[i]) {
            cut = i;
            break;
        }
    }
    if cut == 0 {
        return (text.trim_end().to_string(), String::new());
    }
    (lines[..cut].join("\n").trim_end().to_string(), lines[cut..].join("\n").trim().to_string())
}

/// Parses "Name <a@b>, c@d; ..." into addresses.
pub fn parse_address_list(input: &str) -> Result<Vec<Addr>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut parts = Vec::new();
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' | ';' if !in_quotes => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let (name, email) = match (p.rfind('<'), p.rfind('>')) {
            (Some(a), Some(b)) if a < b => (p[..a].trim().trim_matches('"').trim().to_string(), p[a + 1..b].trim().to_string()),
            _ => (String::new(), p.to_string()),
        };
        let valid = email.contains('@')
            && !email.starts_with('@')
            && !email.ends_with('@')
            && !email.chars().any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>');
        if !valid {
            return Err(Error::Other(format!("invalid email address: {p}")));
        }
        out.push(Addr { name, email });
    }
    Ok(out)
}

pub struct Outgoing<'a> {
    pub from: &'a Addr,
    pub to: &'a [Addr],
    pub cc: &'a [Addr],
    pub subject: &'a str,
    pub text: &'a str,
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [String],
    /// An iCalendar REPLY sent along (answering an invitation).
    pub calendar: Option<&'a str>,
}

// ---- writing (RFC 5322 / 2045 / 2047) ------------------------------------------

/// RFC 2047 encoded-word(s) when needed, so non-ASCII survives any server.
fn encode_header_text(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.to_string();
    }
    // Split on char boundaries so each encoded word stays under 75 chars.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", crate::b64::encode(&chunk)));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", crate::b64::encode(&chunk)));
    }
    words.join("\r\n ")
}

fn format_addr(a: &Addr) -> String {
    if a.name.is_empty() {
        return a.email.clone();
    }
    let atext = |c: char| c.is_ascii_alphanumeric() || c == ' ' || "!#$%&'*+-/=?^_`{|}~".contains(c);
    let name = if a.name.chars().all(atext) {
        a.name.clone()
    } else if a.name.is_ascii() {
        format!("\"{}\"", a.name.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        encode_header_text(&a.name)
    };
    format!("{name} <{}>", a.email)
}

/// "Name: a, b, c" folded at commas to stay under 78 columns.
fn address_header(name: &str, list: &[Addr]) -> String {
    let mut out = format!("{name}: ");
    let mut col = out.len();
    for (i, a) in list.iter().enumerate() {
        let item = format_addr(a);
        if i > 0 {
            out.push(',');
            col += 1;
            if col + item.len() + 1 > 78 {
                out.push_str("\r\n ");
                col = 1;
            } else {
                out.push(' ');
                col += 1;
            }
        }
        out.push_str(&item);
        col += item.len();
    }
    out
}

/// RFC 5322 date, always in UTC ("Mon, 09 Nov 2026 09:30:00 +0000").
pub fn rfc2822_date(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = ts.div_euclid(86400);
    let (y, m, d) = crate::ical::civil_from_days(days);
    let s = ts.rem_euclid(86400);
    format!(
        "{}, {d:02} {} {y} {:02}:{:02}:{:02} +0000",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[m as usize - 1],
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// Quoted-printable (RFC 2045 6.7) with CRLF line endings.
pub fn quoted_printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let bytes = line.as_bytes();
        let mut col = 0;
        for (j, &b) in bytes.iter().enumerate() {
            let last = j + 1 == bytes.len();
            let enc = match b {
                b'=' => true,
                b' ' | b'\t' => last, // trailing whitespace must be encoded
                33..=126 => false,
                _ => true,
            };
            let piece = if enc { format!("={b:02X}") } else { (b as char).to_string() };
            if col + piece.len() > 75 {
                out.push_str("=\r\n");
                col = 0;
            }
            out.push_str(&piece);
            col += piece.len();
        }
    }
    out
}

/// Builds an RFC 5322 message. Returns (Message-ID, bytes).
pub fn build_message(m: &Outgoing<'_>) -> Result<(String, Vec<u8>)> {
    let domain = m.from.email.rsplit('@').next().unwrap_or("localhost");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut rand = [0u8; 12];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut rand).map_err(|_| Error::Other("no randomness".into()))?;
    let mid = format!("{}.{}@{}", now.as_secs(), crate::b64::encode_with(rand, crate::b64::URL_SAFE, false), domain);

    for a in std::iter::once(m.from).chain(m.to).chain(m.cc) {
        crate::net::check_line(&a.name)?;
        crate::net::check_line(&a.email)?;
    }
    for r in m.references.iter().chain(m.in_reply_to.map(|_| &m.from.email)) {
        crate::net::check_line(r)?;
    }
    crate::net::check_line(m.subject)?;

    let mut h = vec![
        format!("Date: {}", rfc2822_date(now.as_secs() as i64)),
        format!("From: {}", format_addr(m.from)),
    ];
    if !m.to.is_empty() {
        h.push(address_header("To", m.to));
    }
    if !m.cc.is_empty() {
        h.push(address_header("Cc", m.cc));
    }
    h.push(format!("Subject: {}", encode_header_text(m.subject)));
    h.push(format!("Message-ID: <{mid}>"));
    if let Some(parent) = m.in_reply_to {
        h.push(format!("In-Reply-To: <{parent}>"));
    }
    if !m.references.is_empty() {
        h.push(format!("References: {}", m.references.iter().map(|r| format!("<{r}>")).collect::<Vec<_>>().join("\r\n ")));
    }
    h.push("MIME-Version: 1.0".into());
    let mut out;
    match m.calendar {
        None => {
            h.push("Content-Type: text/plain; charset=utf-8".into());
            h.push("Content-Transfer-Encoding: quoted-printable".into());
            out = h.join("\r\n");
            out.push_str("\r\n\r\n");
            out.push_str(&quoted_printable(m.text));
            out.push_str("\r\n");
        }
        Some(ics) => {
            // Calendars read the text/calendar alternative; people the text.
            let boundary = format!("=_carteiro_{}", crate::b64::encode_with(&mid, crate::b64::URL_SAFE, false).chars().take(24).collect::<String>());
            h.push(format!("Content-Type: multipart/alternative; boundary=\"{boundary}\""));
            out = h.join("\r\n");
            out.push_str(&format!(
                "\r\n\r\n--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{}\r\n--{boundary}\r\nContent-Type: text/calendar; method=REPLY; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{}\r\n--{boundary}--\r\n",
                quoted_printable(m.text),
                quoted_printable(ics)
            ));
        }
    }
    Ok((mid, out.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] = b"Message-ID: <abc@x>\r\nIn-Reply-To: <p@x>\r\nReferences: <r@x> <p@x>\r\nFrom: Ann Smith <ANN@Example.com>\r\nTo: bob@example.com, \"Cee\" <c@example.com>\r\nSubject: Re: Plans\r\nDate: Tue, 1 Oct 2024 10:00:00 +0000\r\n\r\nSounds good!\r\n\r\nOn Mon, Bob wrote:\r\n> what about friday?\r\n";

    #[test]
    fn parses_meta() {
        let m = parse_meta(RAW);
        assert_eq!(m.message_id, "abc@x");
        assert_eq!(m.in_reply_to, vec!["p@x"]);
        assert_eq!(m.references, vec!["r@x", "p@x"]);
        assert_eq!(m.from, Addr { name: "Ann Smith".into(), email: "ann@example.com".into() });
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.subject, "Re: Plans");
        assert_eq!(m.date, 1727776800);
        assert_eq!(m.snippet, "Sounds good!");
    }

    #[test]
    fn synthesizes_missing_message_id() {
        let m = parse_meta(b"From: a@b\r\nSubject: x\r\n\r\nhi");
        assert!(m.message_id.ends_with("@carteiro.local"));
    }

    #[test]
    fn splits_quotes() {
        let (n, q) = split_quoted("Yes.\n\nOn Tue, 1 Oct 2024, Bob <b@x> wrote:\n> hi\n> there\n");
        assert_eq!(n, "Yes.");
        assert!(q.starts_with("On Tue"));

        let (n, q) = split_quoted("Ok\r\n\r\n-----Original Message-----\r\nFrom: x\r\n");
        assert_eq!(n, "Ok");
        assert!(q.contains("Original"));

        let (n, _) = split_quoted("Ok\n\nFrom: Bob\nSent: Monday\nSubject: x\n");
        assert_eq!(n, "Ok");

        // Interleaved replies keep everything.
        let (n, q) = split_quoted("> q1\nanswer1\n> q2\nanswer2");
        assert_eq!(n, "> q1\nanswer1\n> q2\nanswer2");
        assert!(q.is_empty());
    }

    #[test]
    fn parses_address_lists() {
        let l = parse_address_list("\"Smith, Ann\" <ann@x.org>; bob@y.org,").unwrap();
        assert_eq!(l, vec![Addr { name: "Smith, Ann".into(), email: "ann@x.org".into() }, Addr { name: "".into(), email: "bob@y.org".into() }]);
        assert!(parse_address_list("not an address").is_err());
    }

    #[test]
    fn writes_non_ascii_safely() {
        let from = Addr { name: "Gabriel Carvalho, Jr.".into(), email: "g@x.org".into() };
        let to = vec![Addr { name: "José Ñúñez".into(), email: "j@y.org".into() }];
        let body = "Olá! Preço = 10€\nline with trailing space \n.dot";
        let (_, bytes) = build_message(&Outgoing {
            from: &from,
            to: &to,
            cc: &[],
            subject: "Reunião — próxima semana",
            text: body,
            in_reply_to: None,
            references: &[],
            calendar: None,
        })
        .unwrap();
        assert!(bytes.is_ascii(), "message must be 7-bit on the wire");
        let msg = MessageParser::default().parse(&bytes).unwrap();
        assert_eq!(msg.subject(), Some("Reunião — próxima semana"));
        assert_eq!(msg.body_text(0).unwrap().replace("\r\n", "\n").trim_end_matches('\n'), body);
        let m = parse_meta(&bytes);
        assert_eq!(m.from.name, "Gabriel Carvalho, Jr.");
        assert_eq!(m.to[0].name, "José Ñúñez");
        assert!(m.date > 0);
    }

    #[test]
    fn rejects_header_injection() {
        let from = Addr { name: "x".into(), email: "x@y.org".into() };
        let r = build_message(&Outgoing { from: &from, to: &[], cc: &[], subject: "hi\r\nBcc: victim@z.org", text: "", in_reply_to: None, references: &[], calendar: None });
        assert!(r.is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(rfc2822_date(1794216600), "Mon, 09 Nov 2026 09:30:00 +0000");
    }

    #[test]
    fn builds_reply_that_parses_back() {
        let from = Addr { name: "Me".into(), email: "me@x.org".into() };
        let to = vec![Addr { name: "".into(), email: "you@y.org".into() }];
        let refs = vec!["r@x".to_string(), "p@x".to_string()];
        let (mid, bytes) = build_message(&Outgoing {
            from: &from,
            to: &to,
            cc: &[],
            subject: "Re: Plans",
            text: "Sure",
            in_reply_to: Some("p@x"),
            references: &refs,
            calendar: None,
        })
        .unwrap();
        let m = parse_meta(&bytes);
        assert_eq!(m.message_id, mid);
        assert_eq!(m.in_reply_to, vec!["p@x"]);
        assert_eq!(m.references, refs);
        assert_eq!(m.to, to);
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;

    #[test]
    fn plain_text_mail_has_no_html_body() {
        let plain = parse_body(b"Subject: x\r\nContent-Type: text/plain\r\n\r\nHello\r\n> quoted\r\n");
        assert_eq!(plain.html, None);
        assert!(plain.text.unwrap().contains("> quoted"));
        let alt = parse_body(b"Subject: x\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nHi\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>Hi</p>\r\n--b--\r\n");
        assert_eq!(alt.html.as_deref().map(str::trim), Some("<p>Hi</p>"));
    }
}

#[cfg(test)]
mod injection_tests {
    use super::*;

    #[test]
    fn encoded_line_breaks_never_become_headers() {
        // "Invoice\r\nBcc: attacker@evil.example", hidden in an encoded word.
        let raw = b"From: a@x.org\r\nSubject: =?UTF-8?B?SW52b2ljZQ0KQmNjOiBhdHRhY2tlckBldmlsLmV4YW1wbGU=?=\r\n\r\nbody";
        let meta = parse_meta(raw);
        let from = Addr { name: String::new(), email: "me@x.org".into() };
        let to = [Addr { name: String::new(), email: "a@x.org".into() }];
        let out = Outgoing { from: &from, to: &to, cc: &[], subject: &meta.subject, text: "hi", in_reply_to: None, references: &[], calendar: None };
        match build_message(&out) {
            Err(_) => {}
            Ok((_, msg)) => {
                let headers = String::from_utf8_lossy(&msg).split("\r\n\r\n").next().unwrap_or("").to_ascii_lowercase();
                assert!(!headers.lines().any(|l| l.starts_with("bcc:")), "injected header: {headers}");
            }
        }
    }

    #[test]
    fn writes_invitation_replies_and_names_inline_invitations() {
        let from = Addr { name: "Me".into(), email: "me@y.org".into() };
        let to = [Addr { name: String::new(), email: "ana@x.org".into() }];
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261117T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (_, bytes) = build_message(&Outgoing { from: &from, to: &to, cc: &[], subject: "Accepted: Sync", text: "Me has accepted.", in_reply_to: None, references: &[], calendar: Some(ics) }).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("multipart/alternative") && text.contains("text/calendar; method=REPLY"));
        let parts = crate::invites::calendar_parts(&bytes);
        assert_eq!(parts.len(), 1, "a calendar finds the reply");
        assert_eq!(crate::ical::calendar_method(&parts[0]), "REPLY");
        assert_eq!(parse_body(&bytes).text.as_deref().map(str::trim), Some("Me has accepted."));
        // An invitation sent inline is listed under a useful name.
        let inline = b"From: a@x.org\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n--b--\r\n";
        assert!(parse_body(inline).attachments.iter().any(|a| a.name == "invitation.ics"));
    }
}
