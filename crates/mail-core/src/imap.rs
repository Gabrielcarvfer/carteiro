//! Minimal, strict IMAP4rev1 client: just what a mail client needs, with
//! literal-aware response reading and no plaintext fallback.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::account::{xoauth2, Credential};
use crate::error::{Error, Result};
use crate::imap_proto::{parse_tokens, quote, utf7_decode, Tok};
use crate::net::{self, Conn, Endpoint, MailStream, Security};

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub untagged: Vec<Vec<u8>>,
    pub status: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImapFolder {
    /// Decoded, human readable name.
    pub name: String,
    /// Name as sent on the wire (modified UTF-7).
    pub raw_name: String,
    pub delimiter: Option<String>,
    pub flags: Vec<String>,
}

impl ImapFolder {
    pub fn selectable(&self) -> bool {
        !self.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Noselect") || f.eq_ignore_ascii_case("\\NonExistent"))
    }

    /// RFC 6154 special-use, falling back to common folder names.
    pub fn special_use(&self) -> Option<&'static str> {
        for f in &self.flags {
            let s = match f.to_ascii_lowercase().as_str() {
                "\\sent" => "sent",
                "\\trash" => "trash",
                "\\drafts" => "drafts",
                "\\junk" => "junk",
                "\\archive" => "archive",
                "\\all" => "all",
                "\\flagged" => "flagged",
                "\\important" => "important",
                _ => continue,
            };
            return Some(s);
        }
        if self.name.eq_ignore_ascii_case("INBOX") {
            return Some("inbox");
        }
        let leaf = match &self.delimiter {
            Some(d) if !d.is_empty() => self.name.rsplit(d.as_str()).next().unwrap_or(&self.name),
            _ => self.name.as_str(),
        }
        .to_lowercase();
        match leaf.as_str() {
            "sent" | "sent items" | "sent messages" | "sent mail" => Some("sent"),
            "trash" | "deleted items" | "deleted messages" | "bin" => Some("trash"),
            "drafts" => Some("drafts"),
            "junk" | "spam" | "junk e-mail" | "junk email" => Some("junk"),
            "archive" => Some("archive"),
            _ => None,
        }
    }

    /// Virtual folders that only duplicate mail stored elsewhere.
    pub fn is_virtual(&self) -> bool {
        matches!(self.special_use(), Some("all" | "flagged" | "important"))
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SelectInfo {
    pub exists: u32,
    pub uidvalidity: u32,
    pub uidnext: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub size: u64,
    pub header: Option<Vec<u8>>,
    pub body: Option<Vec<u8>>,
}

impl Fetched {
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

pub struct ImapClient<S> {
    conn: Conn<S>,
    tag: u32,
    pub capabilities: Vec<String>,
}

impl ImapClient<MailStream> {
    pub async fn connect(ep: &Endpoint) -> Result<Self> {
        ep.check_policy()?;
        let tcp = net::connect_tcp(&ep.host, ep.port).await?;
        match ep.security {
            Security::Tls => {
                let tls = net::tls_wrap(tcp, &ep.host).await?;
                let mut c = ImapClient::new(MailStream::Tls(Box::new(tls)));
                c.greeting().await?;
                Ok(c)
            }
            Security::StartTls => {
                let mut c = ImapClient::new(MailStream::Plain(tcp));
                c.greeting().await?;
                c.command_ok("STARTTLS").await?;
                let conn = net::starttls_upgrade(c.conn, &ep.host).await?;
                // Capabilities seen before TLS are untrusted; re-query.
                let mut c = ImapClient { conn, tag: c.tag, capabilities: Vec::new() };
                c.refresh_capabilities().await?;
                Ok(c)
            }
            Security::None => {
                let mut c = ImapClient::new(MailStream::Plain(tcp));
                c.greeting().await?;
                Ok(c)
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> ImapClient<S> {
    pub fn new(stream: S) -> Self {
        Self { conn: Conn::new(stream), tag: 0, capabilities: Vec::new() }
    }

    pub fn has_cap(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c.eq_ignore_ascii_case(cap))
    }

    pub async fn greeting(&mut self) -> Result<()> {
        let line = self.read_item().await?;
        let s = String::from_utf8_lossy(&line).into_owned();
        let up = s.to_ascii_uppercase();
        if !(up.starts_with("* OK") || up.starts_with("* PREAUTH")) {
            return Err(Error::Server(s));
        }
        if let Some(start) = up.find("[CAPABILITY ") {
            if let Some(end) = up[start..].find(']') {
                self.capabilities = s[start + 12..start + end].split_whitespace().map(str::to_string).collect();
            }
        }
        Ok(())
    }

    pub async fn refresh_capabilities(&mut self) -> Result<()> {
        let r = self.command_ok("CAPABILITY").await?;
        for item in &r.untagged {
            let s = String::from_utf8_lossy(item);
            if s.get(..11).is_some_and(|h| h.eq_ignore_ascii_case("CAPABILITY ")) {
                self.capabilities = s[11..].split_whitespace().map(str::to_string).collect();
            }
        }
        Ok(())
    }

    pub async fn login(&mut self, user: &str, pass: &str) -> Result<()> {
        if self.capabilities.is_empty() {
            self.refresh_capabilities().await?;
        }
        // Prefer SASL PLAIN (what Thunderbird calls "normal password"); some
        // servers accept it but reject the legacy LOGIN command.
        let r = if self.has_cap("AUTH=PLAIN") {
            let token = crate::b64::encode(format!("\0{user}\0{pass}"));
            if self.has_cap("SASL-IR") {
                self.command(&format!("AUTHENTICATE PLAIN {token}")).await?
            } else {
                self.command_inner(b"AUTHENTICATE PLAIN", Some(token.as_bytes())).await?
            }
        } else if user.is_ascii() && pass.is_ascii() && !self.has_cap("LOGINDISABLED") {
            self.command(&format!("LOGIN {} {}", quote(user)?, quote(pass)?)).await?
        } else {
            return Err(Error::Auth("server offers no usable login mechanism".into()));
        };
        if r.status != "OK" {
            return Err(Error::Auth(r.text));
        }
        self.refresh_capabilities().await
    }

    pub async fn authenticate(&mut self, user: &str, cred: &Credential) -> Result<()> {
        match cred {
            Credential::Password(p) => self.login(user, p).await,
            Credential::Bearer(token) => {
                let ir = xoauth2(user, token);
                let r = if self.has_cap("SASL-IR") {
                    self.command(&format!("AUTHENTICATE XOAUTH2 {ir}")).await?
                } else {
                    self.command_inner(b"AUTHENTICATE XOAUTH2", Some(ir.as_bytes())).await?
                };
                if r.status != "OK" {
                    return Err(Error::Auth(r.text));
                }
                self.refresh_capabilities().await
            }
        }
    }

    pub async fn command(&mut self, cmd: &str) -> Result<Response> {
        net::check_line(cmd)?;
        self.command_inner(cmd.as_bytes(), None).await
    }

    pub async fn command_ok(&mut self, cmd: &str) -> Result<Response> {
        let r = self.command(cmd).await?;
        if r.status == "OK" {
            Ok(r)
        } else {
            Err(Error::Server(format!("{} {}", r.status, r.text)))
        }
    }

    /// Sends a tagged command. `cont` is sent once when the server asks for a
    /// continuation (literal data or a SASL response).
    async fn command_inner(&mut self, cmd: &[u8], mut cont: Option<&[u8]>) -> Result<Response> {
        self.tag += 1;
        let tag = format!("A{:04}", self.tag);
        let mut line = Vec::with_capacity(tag.len() + cmd.len() + 3);
        line.extend_from_slice(tag.as_bytes());
        line.push(b' ');
        line.extend_from_slice(cmd);
        line.extend_from_slice(b"\r\n");
        self.conn.write_all(&line).await?;

        let mut untagged = Vec::new();
        loop {
            let item = self.read_item().await?;
            if let Some(rest) = item.strip_prefix(b"* ") {
                untagged.push(rest.to_vec());
                continue;
            }
            if item.starts_with(b"+") {
                let mut data = match cont.take() {
                    Some(p) => p.to_vec(),
                    None => b"*".to_vec(), // abort any unexpected SASL exchange
                };
                data.extend_from_slice(b"\r\n");
                self.conn.write_all(&data).await?;
                continue;
            }
            if item.len() > tag.len() && item.starts_with(tag.as_bytes()) && item[tag.len()] == b' ' {
                let rest = String::from_utf8_lossy(&item[tag.len() + 1..]).into_owned();
                let (status, text) = match rest.split_once(' ') {
                    Some((a, b)) => (a.to_ascii_uppercase(), b.to_string()),
                    None => (rest.to_ascii_uppercase(), String::new()),
                };
                return Ok(Response { untagged, status, text });
            }
        }
    }

    /// Reads one response item, inlining any literals it carries.
    async fn read_item(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let line = self.conn.read_line().await?;
            out.extend_from_slice(&line);
            match literal_len(&line) {
                Some(n) => {
                    out.extend_from_slice(b"\r\n");
                    let lit = self.conn.read_exact_n(n).await?;
                    out.extend_from_slice(&lit);
                }
                None => return Ok(out),
            }
        }
    }

    pub async fn list(&mut self) -> Result<Vec<ImapFolder>> {
        let r = self.command_ok("LIST \"\" \"*\"").await?;
        let mut out = Vec::new();
        for item in &r.untagged {
            let toks = parse_tokens(item)?;
            if toks.len() < 4 || !matches!(&toks[0], Tok::Atom(a) if a.eq_ignore_ascii_case("LIST")) {
                continue;
            }
            let flags = match &toks[1] {
                Tok::List(l) => l.iter().filter_map(Tok::as_string).collect(),
                _ => Vec::new(),
            };
            let Some(raw) = toks[3].as_string() else { continue };
            out.push(ImapFolder { name: utf7_decode(&raw), raw_name: raw, delimiter: toks[2].as_string(), flags });
        }
        Ok(out)
    }

    pub async fn select(&mut self, raw_name: &str) -> Result<SelectInfo> {
        let r = self.command_ok(&format!("SELECT {}", quote(raw_name)?)).await?;
        let mut info = SelectInfo::default();
        for item in &r.untagged {
            let up = String::from_utf8_lossy(item).to_ascii_uppercase();
            if let Some(n) = up.strip_suffix(" EXISTS") {
                info.exists = n.trim().parse().unwrap_or(0);
            } else if let Some(v) = resp_code_value(&up, "UIDVALIDITY") {
                info.uidvalidity = v;
            } else if let Some(v) = resp_code_value(&up, "UIDNEXT") {
                info.uidnext = v;
            }
        }
        Ok(info)
    }

    /// UIDs of messages on or after `since` (whole mailbox when None).
    pub async fn uid_search_since(&mut self, since: Option<i64>) -> Result<Vec<u32>> {
        let cmd = match since {
            Some(ts) => format!("UID SEARCH SINCE {}", imap_date(ts)),
            None => "UID SEARCH ALL".to_string(),
        };
        let r = self.command_ok(&cmd).await?;
        let mut out = Vec::new();
        for item in &r.untagged {
            let s = String::from_utf8_lossy(item);
            if let Some(rest) = s.strip_prefix("SEARCH").or_else(|| s.strip_prefix("search")) {
                out.extend(rest.split_whitespace().filter_map(|n| n.parse::<u32>().ok()));
            }
        }
        Ok(out)
    }

    pub async fn uid_fetch(&mut self, uid_set: &str, items: &str) -> Result<Vec<Fetched>> {
        check_uid_set(uid_set)?;
        let r = self.command_ok(&format!("UID FETCH {uid_set} ({items})")).await?;
        let mut out = Vec::new();
        for item in &r.untagged {
            if let Some(f) = parse_fetch(item)? {
                out.push(f);
            }
        }
        Ok(out)
    }

    pub async fn fetch_headers(&mut self, uid_set: &str) -> Result<Vec<Fetched>> {
        self.uid_fetch(uid_set, "UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]").await
    }

    pub async fn fetch_full(&mut self, uid_set: &str) -> Result<Vec<Fetched>> {
        self.uid_fetch(uid_set, "UID FLAGS RFC822.SIZE BODY.PEEK[]").await
    }

    /// `op` is e.g. "+FLAGS.SILENT" or "-FLAGS.SILENT".
    pub async fn uid_store(&mut self, uid_set: &str, op: &str, flags: &str) -> Result<()> {
        check_uid_set(uid_set)?;
        self.command_ok(&format!("UID STORE {uid_set} {op} ({flags})")).await.map(|_| ())
    }

    pub async fn uid_copy(&mut self, uid_set: &str, dest_raw: &str) -> Result<()> {
        check_uid_set(uid_set)?;
        self.command_ok(&format!("UID COPY {uid_set} {}", quote(dest_raw)?)).await.map(|_| ())
    }

    pub async fn uid_move(&mut self, uid_set: &str, dest_raw: &str) -> Result<()> {
        check_uid_set(uid_set)?;
        if self.has_cap("MOVE") {
            return self.command_ok(&format!("UID MOVE {uid_set} {}", quote(dest_raw)?)).await.map(|_| ());
        }
        self.uid_copy(uid_set, dest_raw).await?;
        self.uid_store(uid_set, "+FLAGS.SILENT", "\\Deleted").await?;
        // Plain EXPUNGE would also purge unrelated \Deleted messages, so only
        // expunge when we can target exactly these UIDs.
        if self.has_cap("UIDPLUS") {
            self.command_ok(&format!("UID EXPUNGE {uid_set}")).await?;
        }
        Ok(())
    }

    pub async fn create(&mut self, raw_name: &str) -> Result<()> {
        self.command_ok(&format!("CREATE {}", quote(raw_name)?)).await.map(|_| ())
    }

    pub async fn append(&mut self, raw_folder: &str, flags: &str, msg: &[u8]) -> Result<()> {
        let cmd = format!("APPEND {} ({flags}) {{{}}}", quote(raw_folder)?, msg.len());
        let r = self.command_inner(cmd.as_bytes(), Some(msg)).await?;
        if r.status != "OK" {
            return Err(Error::Server(format!("{} {}", r.status, r.text)));
        }
        Ok(())
    }

    pub async fn logout(mut self) -> Result<()> {
        let _ = self.command("LOGOUT").await;
        Ok(())
    }
}

/// IMAP date ("8-Nov-2026") for SEARCH.
pub fn imap_date(ts: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = crate::ical::civil_from_days(ts.div_euclid(86400));
    format!("{d}-{}-{y}", MONTHS[m as usize - 1])
}

fn literal_len(line: &[u8]) -> Option<usize> {
    if line.last() != Some(&b'}') {
        return None;
    }
    let open = line.iter().rposition(|&b| b == b'{')?;
    let num = std::str::from_utf8(&line[open + 1..line.len() - 1]).ok()?;
    num.trim_end_matches('+').parse().ok()
}

fn resp_code_value(upper: &str, key: &str) -> Option<u32> {
    let needle = format!("[{key} ");
    let start = upper.find(&needle)? + needle.len();
    let end = upper[start..].find(']')? + start;
    upper[start..end].trim().parse().ok()
}

fn check_uid_set(s: &str) -> Result<()> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit() || b == b',' || b == b':' || b == b'*') {
        return Err(Error::Protocol(format!("invalid UID set: {s}")));
    }
    Ok(())
}

/// Parses `<seq> FETCH (...)`.
pub fn parse_fetch(item: &[u8]) -> Result<Option<Fetched>> {
    let toks = parse_tokens(item)?;
    if toks.len() < 3 || !matches!(&toks[1], Tok::Atom(a) if a.eq_ignore_ascii_case("FETCH")) {
        return Ok(None);
    }
    let Tok::List(list) = &toks[2] else { return Ok(None) };
    let mut f = Fetched::default();
    let mut it = list.iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        let Some(key) = k.as_string() else { continue };
        match key.to_ascii_uppercase().as_str() {
            "UID" => f.uid = v.as_string().and_then(|s| s.parse().ok()).unwrap_or(0),
            "RFC822.SIZE" => f.size = v.as_string().and_then(|s| s.parse().ok()).unwrap_or(0),
            "FLAGS" => {
                if let Tok::List(l) = v {
                    f.flags = l.iter().filter_map(Tok::as_string).collect();
                }
            }
            "BODY[HEADER]" | "RFC822.HEADER" => f.header = v.as_bytes(),
            "BODY[]" | "RFC822" => f.body = v.as_bytes(),
            _ => {}
        }
    }
    Ok(if f.uid == 0 { None } else { Some(f) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    /// Scripted fake server: for each step, optionally read one client line
    /// and assert it equals `expect`, then write `reply`.
    async fn serve(server: DuplexStream, script: Vec<(&'static str, &'static [u8])>) {
        let mut io = BufReader::new(server);
        for (expect, reply) in script {
            if !expect.is_empty() {
                let mut line = String::new();
                io.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), expect);
            }
            io.get_mut().write_all(reply).await.unwrap();
        }
    }

    #[tokio::test]
    async fn login_select_fetch() {
        let (client, server) = tokio::io::duplex(1 << 16);
        let srv = tokio::spawn(serve(
            server,
            vec![
                ("", b"* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] hi\r\n"),
                ("A0001 AUTHENTICATE PLAIN", b"+ \r\n"),
                ("AG1lAHB3", b"A0001 OK logged in\r\n"),
                ("A0002 CAPABILITY", b"* CAPABILITY IMAP4rev1 MOVE UIDPLUS\r\nA0002 OK\r\n"),
                (
                    "A0003 SELECT \"INBOX\"",
                    b"* 2 EXISTS\r\n* OK [UIDVALIDITY 42] v\r\n* OK [UIDNEXT 7] n\r\nA0003 OK [READ-WRITE] done\r\n",
                ),
                (
                    "A0004 UID FETCH 5:* (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])",
                    b"* 1 FETCH (UID 5 FLAGS (\\Seen) RFC822.SIZE 99 BODY[HEADER] {26}\r\nSubject: hi\r\nFrom: a@b\r\n\r\n)\r\nA0004 OK\r\n",
                ),
                ("A0005 UID MOVE 5 \"Archive\"", b"A0005 OK moved\r\n"),
            ],
        ));
        let mut c = ImapClient::new(client);
        c.greeting().await.unwrap();
        c.login("me", "pw").await.unwrap();
        assert!(c.has_cap("move"));
        let info = c.select("INBOX").await.unwrap();
        assert_eq!(info, SelectInfo { exists: 2, uidvalidity: 42, uidnext: 7 });
        let f = c.fetch_headers("5:*").await.unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].uid, 5);
        assert!(f[0].has_flag("\\seen"));
        assert_eq!(f[0].header.as_deref(), Some(&b"Subject: hi\r\nFrom: a@b\r\n\r\n"[..]));
        c.uid_move("5", "Archive").await.unwrap();
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn failed_login_is_auth_error() {
        let (client, server) = tokio::io::duplex(1 << 12);
        let srv = tokio::spawn(serve(
            server,
            vec![
                ("", b"* OK [CAPABILITY IMAP4rev1] hi\r\n"),
                ("A0001 LOGIN \"me\" \"bad\"", b"A0001 NO [AUTHENTICATIONFAILED] nope\r\n"),
            ],
        ));
        let mut c = ImapClient::new(client);
        c.greeting().await.unwrap();
        assert!(matches!(c.login("me", "bad").await, Err(Error::Auth(_))));
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn append_sends_literal_after_continuation() {
        let (client, server) = tokio::io::duplex(1 << 12);
        let srv = tokio::spawn(serve(
            server,
            vec![
                ("", b"* OK hi\r\n"),
                ("A0001 APPEND \"Sent\" (\\Seen) {5}", b"+ go\r\n"),
                ("hello", b"A0001 OK appended\r\n"),
            ],
        ));
        let mut c = ImapClient::new(client);
        c.greeting().await.unwrap();
        c.append("Sent", "\\Seen", b"hello").await.unwrap();
        srv.await.unwrap();
    }

    #[test]
    fn search_dates() {
        assert_eq!(imap_date(1794216600), "9-Nov-2026");
    }

    #[test]
    fn uid_set_validation() {
        assert!(check_uid_set("1:*").is_ok());
        assert!(check_uid_set("1,2,3").is_ok());
        assert!(check_uid_set("1 FLAGS").is_err());
    }

    #[test]
    fn special_use_detection() {
        let f = |name: &str, flags: &[&str]| ImapFolder {
            name: name.into(),
            raw_name: name.into(),
            delimiter: Some("/".into()),
            flags: flags.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(f("[Gmail]/Sent Mail", &["\\Sent"]).special_use(), Some("sent"));
        assert_eq!(f("Deleted Items", &[]).special_use(), Some("trash"));
        assert!(f("[Gmail]/All Mail", &["\\All"]).is_virtual());
        assert!(!f("[Gmail]", &["\\Noselect"]).selectable());
    }
}
