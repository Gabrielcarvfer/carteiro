//! Contacts: the model and the file formats people bring them in:
//! vCard (Google, Outlook, Apple, Thunderbird; 2.1, 3.0 and 4.0), CSV with
//! Google's, Outlook's or Thunderbird's columns, and Thunderbird's LDIF.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    pub uid: String,
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub organization: String,
    pub notes: String,
}

/// Whether an address looks like a person rather than a robot (no-reply,
/// notifications, bounces, mailing-list machinery).
pub fn is_person_address(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else { return false };
    if local.is_empty() || !domain.contains('.') || email.len() > 254 || email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let l = local.to_ascii_lowercase();
    const ROBOTS: &[&str] = &[
        "noreply", "no-reply", "no_reply", "donotreply", "do-not-reply", "do_not_reply", "notification", "notifications", "notify",
        "mailer-daemon", "postmaster", "bounce", "bounces", "automated", "auto-confirm", "alerts", "alert", "newsletter", "news",
        "mailer", "daemon", "unsubscribe", "info-noreply", "updates", "digest", "system",
    ];
    let robot = ROBOTS.iter().any(|r| l == *r || l.starts_with(&format!("{r}-")) || l.starts_with(&format!("{r}+")))
        || l.contains("noreply")
        || l.contains("no-reply")
        || l.starts_with("bounce")
        // Variable envelope addresses: long random-looking local parts.
        || (l.len() > 40 && l.chars().filter(|c| c.is_ascii_digit()).count() > 10);
    !robot
}

impl Contact {
    /// Trims fields, drops empty and duplicate values, fills a missing name.
    pub fn normalized(mut self) -> Option<Contact> {
        let clean = |v: Vec<String>, email: bool| {
            let mut out: Vec<String> = Vec::new();
            for x in v {
                let x = x.trim().trim_start_matches("mailto:").trim().to_string();
                let x = if email { x.to_ascii_lowercase() } else { x };
                if !x.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&x)) && (!email || x.contains('@')) {
                    out.push(x);
                }
            }
            out
        };
        self.emails = clean(self.emails, true);
        self.phones = clean(self.phones, false);
        self.name = self.name.split_whitespace().collect::<Vec<_>>().join(" ");
        self.organization = self.organization.trim().to_string();
        self.notes = self.notes.trim().to_string();
        if self.name.is_empty() {
            self.name = self.emails.first().cloned().or_else(|| (!self.organization.is_empty()).then(|| self.organization.clone())).unwrap_or_default();
        }
        (!self.name.is_empty() || !self.phones.is_empty()).then_some(self)
    }

    /// Same person, by any shared email (or same name when neither has one).
    pub fn same_as(&self, other: &Contact) -> bool {
        if self.emails.is_empty() && other.emails.is_empty() {
            return !self.name.is_empty() && self.name.eq_ignore_ascii_case(&other.name);
        }
        self.emails.iter().any(|e| other.emails.iter().any(|o| o.eq_ignore_ascii_case(e)))
    }

    /// Adds what `other` knows that this one doesn't.
    pub fn merge(&mut self, other: &Contact) {
        for e in &other.emails {
            if !self.emails.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                self.emails.push(e.clone());
            }
        }
        for p in &other.phones {
            if !self.phones.contains(p) {
                self.phones.push(p.clone());
            }
        }
        if self.organization.is_empty() {
            self.organization = other.organization.clone();
        }
        if self.notes.is_empty() {
            self.notes = other.notes.clone();
        } else if !other.notes.is_empty() && !self.notes.contains(&other.notes) {
            self.notes = format!("{}\n{}", self.notes, other.notes);
        }
        if self.name.contains('@') && !other.name.is_empty() && !other.name.contains('@') {
            self.name = other.name.clone();
        }
    }
}

/// Parses a contacts file, recognising the format from its content.
pub fn parse_any(text: &str) -> Vec<Contact> {
    let text = text.trim_start_matches('\u{feff}');
    let head = text.trim_start().get(..64).unwrap_or(text.trim_start()).to_ascii_uppercase();
    let contacts = if head.starts_with("BEGIN:VCARD") {
        parse_vcards(text)
    } else if text.lines().any(|l| l.to_ascii_lowercase().starts_with("dn:") || l.to_ascii_lowercase().starts_with("dn::")) {
        parse_ldif(text)
    } else {
        parse_csv(text)
    };
    contacts.into_iter().filter_map(Contact::normalized).collect()
}

// ---- vCard ----------------------------------------------------------------------------

fn unescape_vcard(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_vcard(v: &str) -> String {
    v.replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace('\r', "").replace('\n', "\\n")
}

/// vCard 2.1 quoted-printable ("=C3=A9", soft line breaks already joined).
fn decode_qp(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' && i + 2 < b.len() {
            if let Ok(x) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a value on unescaped `sep` (N and ORG are ';'-separated lists).
fn split_unescaped(v: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut esc = false;
    for c in v.chars() {
        if esc {
            parts.last_mut().unwrap().push('\\');
            parts.last_mut().unwrap().push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == sep {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
    }
    parts.into_iter().map(|p| unescape_vcard(&p)).collect()
}

pub fn parse_vcards(text: &str) -> Vec<Contact> {
    // Unfold: a line starting with space/tab continues the previous one; a
    // quoted-printable line ending in '=' continues on the next.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n').map(|l| l.trim_end_matches('\r')) {
        match lines.last_mut() {
            Some(prev) if raw.starts_with([' ', '\t']) => prev.push_str(&raw[1..]),
            Some(prev) if prev.ends_with('=') && prev.to_ascii_uppercase().contains("QUOTED-PRINTABLE") => {
                prev.pop();
                prev.push_str(raw);
            }
            _ => lines.push(raw.to_string()),
        }
    }
    let mut out = Vec::new();
    let mut cur: Option<(Contact, Vec<String>)> = None;
    for line in lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut params = head.split(';');
        // Property names may carry a group prefix ("item1.EMAIL").
        let name = params.next().unwrap_or("").rsplit('.').next().unwrap_or("").to_ascii_uppercase();
        let params: Vec<String> = params.map(|p| p.to_ascii_uppercase()).collect();
        let value = if params.iter().any(|p| p.contains("QUOTED-PRINTABLE")) { decode_qp(value) } else { value.to_string() };
        match name.as_str() {
            "BEGIN" if value.eq_ignore_ascii_case("VCARD") => cur = Some((Contact::default(), Vec::new())),
            "END" if value.eq_ignore_ascii_case("VCARD") => {
                if let Some((mut c, n)) = cur.take() {
                    if c.name.is_empty() {
                        // N: Family;Given;Additional;Prefix;Suffix
                        let get = |i: usize| n.get(i).map(String::as_str).unwrap_or("");
                        c.name = [get(3), get(1), get(2), get(0), get(4)].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
                    }
                    out.push(c);
                }
            }
            _ => {
                let Some((c, n)) = cur.as_mut() else { continue };
                match name.as_str() {
                    "FN" => c.name = unescape_vcard(&value),
                    "N" => *n = split_unescaped(&value, ';'),
                    "EMAIL" => c.emails.push(unescape_vcard(&value)),
                    "TEL" => c.phones.push(unescape_vcard(&value).trim_start_matches("tel:").to_string()),
                    "ORG" => c.organization = split_unescaped(&value, ';').into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
                    "NOTE" => c.notes = unescape_vcard(&value),
                    "UID" => c.uid = unescape_vcard(&value),
                    _ => {}
                }
            }
        }
    }
    out
}

/// One vCard 3.0, folded at 75 octets.
pub fn to_vcard(c: &Contact) -> String {
    let mut lines = vec!["BEGIN:VCARD".to_string(), "VERSION:3.0".into(), format!("FN:{}", escape_vcard(&c.name)), format!("N:;{};;;", escape_vcard(&c.name))];
    if !c.uid.is_empty() {
        lines.push(format!("UID:{}", escape_vcard(&c.uid)));
    }
    lines.extend(c.emails.iter().map(|e| format!("EMAIL;TYPE=INTERNET:{}", escape_vcard(e))));
    lines.extend(c.phones.iter().map(|p| format!("TEL:{}", escape_vcard(p))));
    if !c.organization.is_empty() {
        lines.push(format!("ORG:{}", escape_vcard(&c.organization)));
    }
    if !c.notes.is_empty() {
        lines.push(format!("NOTE:{}", escape_vcard(&c.notes)));
    }
    lines.push("END:VCARD".into());
    lines.iter().map(|l| fold(l)).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for ch in line.chars() {
        if len + ch.len_utf8() > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(ch);
        len += ch.len_utf8();
    }
    out
}

// ---- CSV (Google, Outlook, Thunderbird) ---------------------------------------------

/// RFC 4180 records (quoted fields, doubled quotes, newlines inside quotes).
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            ('"', true) => quoted = false,
            ('"', false) if field.is_empty() => quoted = true,
            (',', false) => row.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (c, _) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

pub fn parse_csv(text: &str) -> Vec<Contact> {
    let rows = csv_records(text);
    let Some((header, body)) = rows.split_first() else { return Vec::new() };
    let h: Vec<String> = header.iter().map(|x| x.trim().to_ascii_lowercase()).collect();
    let col = |names: &[&str]| h.iter().position(|x| names.contains(&x.as_str()));
    let (name, first, middle, last) = (
        col(&["name", "display name", "full name"]),
        col(&["first name", "given name"]),
        col(&["middle name", "additional name"]),
        col(&["last name", "family name", "surname"]),
    );
    let org = col(&["company", "organization 1 - name", "organization", "organization name", "organisation"]);
    let notes = col(&["notes", "note"]);
    // Every email / phone value column, in order ("E-mail 2 Address",
    // "E-mail 1 - Value", "Primary Email", "Mobile Phone", "Phone 1 - Value"...).
    let is_value = |x: &str| !x.contains("type") && !x.contains("label") && !x.contains("display");
    let emails: Vec<usize> = h.iter().enumerate().filter(|(_, x)| (x.contains("e-mail") || x.contains("email")) && is_value(x)).map(|(i, _)| i).collect();
    let phones: Vec<usize> = h
        .iter()
        .enumerate()
        .filter(|(_, x)| (x.contains("phone") || x.contains("mobile") || x.contains("pager") || x.contains("fax")) && is_value(x) && !x.contains("e-mail"))
        .map(|(i, _)| i)
        .collect();
    let mut out = Vec::new();
    for r in body {
        let get = |i: Option<usize>| i.and_then(|i| r.get(i)).map(|s| s.trim().to_string()).unwrap_or_default();
        // Google packs several values in one cell: "a@x ::: b@y".
        let many = |cols: &[usize]| cols.iter().flat_map(|&i| r.get(i).map(|v| v.split(":::").map(|s| s.trim().to_string()).collect::<Vec<_>>()).unwrap_or_default()).collect();
        let mut n = get(name);
        if n.is_empty() {
            n = [get(first), get(middle), get(last)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        }
        out.push(Contact { name: n, emails: many(&emails), phones: many(&phones), organization: get(org), notes: get(notes), ..Default::default() });
    }
    out
}

// ---- LDIF (Thunderbird) ---------------------------------------------------------------

pub fn parse_ldif(text: &str) -> Vec<Contact> {
    // Unfold (continuation lines start with a space), split into entries.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n').map(|l| l.trim_end_matches('\r')) {
        match lines.last_mut() {
            Some(prev) if raw.starts_with(' ') && !prev.is_empty() => prev.push_str(&raw[1..]),
            _ => lines.push(raw.to_string()),
        }
    }
    let mut out = Vec::new();
    let mut cur = Contact::default();
    let mut given = (String::new(), String::new());
    let mut flush = |c: &mut Contact, g: &mut (String, String)| {
        if c.name.is_empty() {
            c.name = format!("{} {}", g.0, g.1).trim().to_string();
        }
        if *c != Contact::default() {
            out.push(std::mem::take(c));
        }
        *g = (String::new(), String::new());
    };
    for line in lines {
        if line.trim().is_empty() {
            flush(&mut cur, &mut given);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // "attr: value" or "attr:: base64"
        let Some((attr, rest)) = line.split_once(':') else { continue };
        let value = match rest.strip_prefix(':') {
            Some(b64) => crate::b64::decode(b64.trim()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
            None => rest.trim().to_string(),
        };
        match attr.to_ascii_lowercase().as_str() {
            "cn" | "displayname" => cur.name = value,
            "givenname" => given.0 = value,
            "sn" => given.1 = value,
            "mail" | "mozillasecondemail" | "mozilla_alternateemail" => cur.emails.push(value),
            "telephonenumber" | "mobile" | "homephone" | "pager" | "facsimiletelephonenumber" => cur.phones.push(value),
            "o" | "company" => cur.organization = value,
            "description" => cur.notes = value,
            _ => {}
        }
    }
    flush(&mut cur, &mut given);
    out
}

// ---- syncing ------------------------------------------------------------------------

/// Google's CardDAV address book for an account.
pub fn google_carddav_url(email: &str) -> String {
    format!("https://www.googleapis.com/carddav/v1/principals/{}/lists/default/", crate::http::encode(email))
}

/// Mirrors a synced address book (all of its books, for CardDAV). Returns
/// how many contacts it has.
pub async fn sync_source(store: &std::sync::Mutex<crate::store::Store>, source: &crate::store::ContactSource, cred: &crate::account::Credential) -> crate::error::Result<usize> {
    use crate::error::Error;
    let list: Vec<(String, Option<String>, Contact)> = match source.kind.as_str() {
        "microsoft" => {
            let crate::account::Credential::Bearer(token) = cred else { return Err(Error::Other("Microsoft contacts need a sign-in".into())) };
            crate::graph::contacts(token).await?.into_iter().map(|(id, c)| (id, None, c)).collect()
        }
        _ => {
            let dav = crate::caldav::CalDav { user: source.username.clone(), cred: cred.clone() };
            let mut all = Vec::new();
            for book in crate::carddav::discover(&dav, &source.url).await? {
                all.extend(crate::carddav::contacts(&dav, &book.url).await?);
            }
            all
        }
    };
    let n = list.len();
    store.lock().map_err(|_| Error::Other("store lock poisoned".into()))?.replace_contacts(source.id, &list)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_not_robots() {
        for p in ["ana@example.org", "bruno.lima@corp.example", "news.anchor@tv.example"] {
            assert!(is_person_address(p), "{p}");
        }
        for r in ["noreply@x.org", "no-reply@accounts.example", "notifications@github.example", "MAILER-DAEMON@x.org",
            "bounce-123@mail.example", "do-not-reply@bank.example", "shop-noreply@x.org", "x", "a@b", "a b@c.org"] {
            assert!(!is_person_address(r), "{r}");
        }
    }

    #[test]
    fn vcards_from_google_apple_and_outlook_2_1() {
        let v = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ana Souza\r\nN:Souza;Ana;;;\r\nitem1.EMAIL;TYPE=INTERNET:ana@example.org\r\nEMAIL;TYPE=WORK:Ana@Work.example\r\nTEL;TYPE=CELL:+34 600 000 000\r\nORG:Acme\\, Inc.;Sales\r\nNOTE:Met at\\nthe conference\r\nUID:abc-1\r\nEND:VCARD\r\n\
BEGIN:VCARD\r\nVERSION:2.1\r\nN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Pere=\r\nira;Jo=C3=A3o\r\nEMAIL;PREF;INTERNET:joao@example.org\r\nEND:VCARD\r\n\
BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Bo\r\n  Long Name\r\nEMAIL:mailto:bo@example.org\r\nTEL;VALUE=uri:tel:+1-555-0100\r\nEND:VCARD\r\n";
        let c = parse_any(v);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].name, "Ana Souza");
        assert_eq!(c[0].emails, ["ana@example.org", "ana@work.example"]);
        assert_eq!(c[0].organization, "Acme, Inc., Sales");
        assert_eq!(c[0].notes, "Met at\nthe conference");
        assert_eq!(c[1].name, "João Pereira", "vCard 2.1 quoted-printable N");
        assert_eq!((c[2].name.as_str(), c[2].emails[0].as_str(), c[2].phones[0].as_str()), ("Bo Long Name", "bo@example.org", "+1-555-0100"));
    }

    #[test]
    fn csv_from_google_outlook_and_thunderbird() {
        let google = "Name,Given Name,Family Name,E-mail 1 - Type,E-mail 1 - Value,Phone 1 - Type,Phone 1 - Value,Organization 1 - Name,Notes\n\
\"Souza, Ana\",Ana,Souza,* Home,ana@example.org ::: ana@work.example,Mobile,+34 600,Acme,\"line 1\nline 2\"\n";
        let outlook = "First Name,Middle Name,Last Name,E-mail Address,E-mail 2 Address,Mobile Phone,Business Phone,Company\r\nBob,,Silva,bob@example.org,,+1 555,,Initech\r\n";
        let thunderbird = "First Name,Last Name,Display Name,Nickname,Primary Email,Secondary Email,Work Phone,Mobile Number\nCarol,P,Carol Pereira,,carol@example.org,c@alt.example,,+351 91\n";
        let g = parse_any(google);
        assert_eq!((g[0].name.as_str(), g[0].emails.len(), g[0].notes.as_str()), ("Souza, Ana", 2, "line 1\nline 2"));
        assert_eq!(g[0].phones, ["+34 600"]);
        let o = parse_any(outlook);
        assert_eq!((o[0].name.as_str(), o[0].emails.as_slice(), o[0].organization.as_str()), ("Bob Silva", &["bob@example.org".to_string()][..], "Initech"));
        let t = parse_any(thunderbird);
        assert_eq!((t[0].name.as_str(), t[0].emails.len(), t[0].phones.as_slice()), ("Carol Pereira", 2, &["+351 91".to_string()][..]));
    }

    #[test]
    fn ldif_from_thunderbird() {
        let ldif = "dn: cn=Dave Costa,mail=dave@example.org\nobjectclass: top\ncn: Dave Costa\nmail: dave@example.org\nmozillaSecondEmail: d@alt.example\ntelephoneNumber: 555 1234\no: Globex\n\n\
dn:: Y249Sm/Do28=\ncn:: Sm/Do28gUGVyZWlyYQ==\nmail: joao@example.org\ndescription: long\n  note\n";
        let c = parse_any(ldif);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].name.as_str(), c[0].emails.len(), c[0].organization.as_str()), ("Dave Costa", 2, "Globex"));
        assert_eq!((c[1].name.as_str(), c[1].notes.as_str()), ("João Pereira", "long note"), "base64 and folded values");
    }

    #[test]
    fn vcard_roundtrip_and_merging() {
        let c = Contact { uid: "u1".into(), name: "Ana; Souza, Jr".into(), emails: vec!["ana@example.org".into()], phones: vec!["+34 600".into()], organization: "Acme".into(), notes: "a\nb".into() };
        let back = parse_vcards(&to_vcard(&c));
        assert_eq!(back[0], c);
        let mut a = Contact { name: "ana@example.org".into(), emails: vec!["ana@example.org".into()], ..Default::default() };
        let b = Contact { name: "Ana Souza".into(), emails: vec!["ANA@example.org".into(), "ana@work.example".into()], phones: vec!["1".into()], ..Default::default() };
        assert!(a.same_as(&b));
        a.merge(&b);
        assert_eq!((a.name.as_str(), a.emails.len(), a.phones.len()), ("Ana Souza", 2, 1));
        assert!(Contact::default().normalized().is_none(), "empty rows are dropped");
    }
}
