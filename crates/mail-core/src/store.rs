//! Local SQLite cache: accounts, folders, messages, threads and rules.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::error::{Error, Result};
use crate::message::{parse_body, Addr, MessageMeta};
use crate::rules::Rule;
use crate::threading::{is_reply_subject, normalize_subject};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (id INTEGER PRIMARY KEY, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS folders (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    raw_name TEXT NOT NULL,
    delimiter TEXT,
    special TEXT,
    uidvalidity INTEGER NOT NULL DEFAULT 0,
    last_uid INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, name)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder TEXT NOT NULL,
    uid INTEGER,
    pop_uidl TEXT,
    message_id TEXT NOT NULL,
    thread_key TEXT NOT NULL,
    norm_subject TEXT NOT NULL,
    subject TEXT NOT NULL,
    from_name TEXT NOT NULL,
    from_email TEXT NOT NULL,
    recipients TEXT NOT NULL,
    date INTEGER NOT NULL,
    seen INTEGER NOT NULL DEFAULT 0,
    flagged INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    snippet TEXT NOT NULL DEFAULT '',
    raw BLOB
);
CREATE INDEX IF NOT EXISTS idx_msg_folder ON messages(account_id, folder, date DESC);
CREATE INDEX IF NOT EXISTS idx_msg_thread ON messages(account_id, thread_key);
CREATE INDEX IF NOT EXISTS idx_msg_mid ON messages(account_id, message_id);
CREATE INDEX IF NOT EXISTS idx_msg_subject ON messages(account_id, norm_subject);
CREATE INDEX IF NOT EXISTS idx_msg_uid ON messages(account_id, folder, uid);
CREATE TABLE IF NOT EXISTS msg_refs (
    msg INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ref_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_refs_ref ON msg_refs(ref_id);
-- Search text of each downloaded message, kept only when ClamAV found it
-- clean (status 'clean'); 'infected' rows record that it must not be indexed.
CREATE TABLE IF NOT EXISTS msg_text (
    msg INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    body TEXT
);
CREATE INDEX IF NOT EXISTS idx_refs_msg ON msg_refs(msg);
-- Content-addressed attachment storage: each distinct attachment body is
-- stored once; messages.raw holds the message with those byte ranges cut out.
CREATE TABLE IF NOT EXISTS blobs (hash TEXT PRIMARY KEY, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS msg_blobs (
    msg INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY (msg, seq)
);
CREATE INDEX IF NOT EXISTS idx_msg_blobs_hash ON msg_blobs(hash);
-- Messages waiting to be sent (offline or server unreachable).
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    msg INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    rcpts TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);
-- Calendars: a source (local, CalDAV server, Google or Microsoft login) has
-- calendars, which have events (recurrences stored as expanded instances).
CREATE TABLE IF NOT EXISTS cal_sources (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    account_id INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
    url TEXT NOT NULL DEFAULT '',
    username TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS calendars (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES cal_sources(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    name TEXT NOT NULL,
    color TEXT,
    read_only INTEGER NOT NULL DEFAULT 0,
    visible INTEGER NOT NULL DEFAULT 1,
    UNIQUE (source_id, remote_id)
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    calendar_id INTEGER NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL DEFAULT '',
    etag TEXT,
    uid TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    all_day INTEGER NOT NULL DEFAULT 0,
    floating INTEGER NOT NULL DEFAULT 0,
    recurring INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_events_time ON events(start, end);
CREATE INDEX IF NOT EXISTS idx_events_cal ON events(calendar_id);
-- Server actions made while offline, replayed in order on the next sync.
CREATE TABLE IF NOT EXISTS pending_ops (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS rules (id INTEGER PRIMARY KEY, position INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS secrets (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FolderInfo {
    pub name: String,
    pub raw_name: String,
    pub delimiter: Option<String>,
    pub special: Option<String>,
    pub unread: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredMessage {
    pub id: i64,
    pub account_id: i64,
    pub folder: String,
    pub uid: Option<u32>,
    pub message_id: String,
    pub thread_key: String,
    pub subject: String,
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub date: i64,
    pub seen: bool,
    pub flagged: bool,
    pub has_attachments: bool,
    pub snippet: String,
    pub has_raw: bool,
    pub size: u64,
}

impl StoredMessage {
    pub fn to_meta(&self) -> MessageMeta {
        MessageMeta {
            message_id: self.message_id.clone(),
            subject: self.subject.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            date: self.date,
            has_attachments: self.has_attachments,
            snippet: self.snippet.clone(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThreadSummary {
    pub thread_key: String,
    pub subject: String,
    pub participants: Vec<String>,
    pub snippet: String,
    pub last_date: i64,
    pub count: u32,
    pub unread: u32,
    pub flagged: bool,
    pub has_attachments: bool,
    pub latest_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalSource {
    pub id: i64,
    /// "local", "caldav", "google" or "microsoft".
    pub kind: String,
    pub account_id: Option<i64>,
    pub url: String,
    pub username: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalendarInfo {
    pub id: i64,
    pub source_id: i64,
    pub source_kind: String,
    pub source_name: String,
    pub remote_id: String,
    pub name: String,
    pub color: Option<String>,
    pub read_only: bool,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredEvent {
    pub id: i64,
    pub calendar_id: i64,
    pub remote_id: String,
    pub etag: Option<String>,
    #[serde(flatten)]
    pub event: crate::ical::Event,
}

pub struct NewMessage {
    pub account_id: i64,
    pub folder: String,
    pub uid: Option<u32>,
    pub pop_uidl: Option<String>,
    pub meta: MessageMeta,
    pub seen: bool,
    pub flagged: bool,
    pub size: u64,
    pub raw: Option<Vec<u8>>,
}

#[derive(Serialize)]
struct RecipientsRef<'a> {
    to: &'a [Addr],
    cc: &'a [Addr],
}

#[derive(Deserialize, Default)]
struct Recipients {
    #[serde(default)]
    to: Vec<Addr>,
    #[serde(default)]
    cc: Vec<Addr>,
}

const MSG_COLS: &str = "id, account_id, folder, uid, message_id, thread_key, subject, from_name, from_email, recipients, date, seen, flagged, has_attachments, snippet, raw IS NOT NULL, size";

fn row_to_msg(r: &Row) -> rusqlite::Result<StoredMessage> {
    let rcpt: Recipients = serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default();
    Ok(StoredMessage {
        id: r.get(0)?,
        account_id: r.get(1)?,
        folder: r.get(2)?,
        uid: r.get(3)?,
        message_id: r.get(4)?,
        thread_key: r.get(5)?,
        subject: r.get(6)?,
        from: Addr { name: r.get(7)?, email: r.get(8)? },
        to: rcpt.to,
        cc: rcpt.cc,
        date: r.get(10)?,
        seen: r.get(11)?,
        flagged: r.get(12)?,
        has_attachments: r.get(13)?,
        snippet: r.get(14)?,
        has_raw: r.get(15)?,
        size: r.get::<_, i64>(16)? as u64,
    })
}

fn folder_rank(f: &FolderInfo) -> (u8, String) {
    let rank = match f.special.as_deref() {
        Some("inbox") => 0,
        Some("drafts") => 1,
        Some("sent") => 2,
        Some("archive") => 3,
        Some("junk") => 4,
        Some("trash") => 5,
        Some("outbox") => 1,
        _ => 6,
    };
    (rank, f.name.to_lowercase())
}

fn like_pattern(q: &str) -> String {
    let escaped = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Columns added after the first release.
        let has_tzid = conn.prepare("SELECT tzid FROM events LIMIT 0").is_ok();
        if !has_tzid {
            conn.execute_batch("ALTER TABLE events ADD COLUMN tzid TEXT NOT NULL DEFAULT ''")?;
        }
        Ok(Self { conn })
    }

    // ---- accounts -------------------------------------------------------

    pub fn add_account(&self, a: &mut Account) -> Result<()> {
        self.conn.execute("INSERT INTO accounts (data) VALUES ('{}')", [])?;
        a.id = self.conn.last_insert_rowid();
        self.update_account(a)
    }

    pub fn update_account(&self, a: &Account) -> Result<()> {
        self.conn.execute("UPDATE accounts SET data = ?1 WHERE id = ?2", params![serde_json::to_string(a)?, a.id])?;
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let mut st = self.conn.prepare("SELECT id, data FROM accounts ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data) = row?;
            let mut a: Account = serde_json::from_str(&data)?;
            a.id = id;
            out.push(a);
        }
        Ok(out)
    }

    pub fn get_account(&self, id: i64) -> Result<Option<Account>> {
        Ok(self.list_accounts()?.into_iter().find(|a| a.id == id))
    }

    pub fn delete_account(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM accounts WHERE id = ?1", [id])?;
        self.gc_blobs()?;
        Ok(())
    }

    // ---- folders --------------------------------------------------------

    pub fn upsert_folder(&self, account_id: i64, name: &str, raw_name: &str, delimiter: Option<&str>, special: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO folders (account_id, name, raw_name, delimiter, special) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, name) DO UPDATE SET raw_name = excluded.raw_name, delimiter = excluded.delimiter, special = excluded.special",
            params![account_id, name, raw_name, delimiter, special],
        )?;
        Ok(())
    }

    /// Drops folders (and their cached mail) that no longer exist on the server.
    pub fn retain_folders(&self, account_id: i64, keep: &[String]) -> Result<()> {
        // Local-only folders (the Outbox) are never on the server.
        let existing: Vec<String> = self
            .list_folders(account_id)?
            .into_iter()
            .filter(|f| f.special.as_deref() != Some("outbox"))
            .map(|f| f.name)
            .collect();
        for name in existing.iter().filter(|n| !keep.contains(n)) {
            self.conn.execute("DELETE FROM messages WHERE account_id = ?1 AND folder = ?2", params![account_id, name])?;
            self.conn.execute("DELETE FROM folders WHERE account_id = ?1 AND name = ?2", params![account_id, name])?;
        }
        Ok(())
    }

    pub fn list_folders(&self, account_id: i64) -> Result<Vec<FolderInfo>> {
        let mut st = self.conn.prepare(
            "SELECT f.name, f.raw_name, f.delimiter, f.special,
                (SELECT COUNT(*) FROM messages m WHERE m.account_id = f.account_id AND m.folder = f.name AND m.seen = 0),
                (SELECT COUNT(*) FROM messages m WHERE m.account_id = f.account_id AND m.folder = f.name)
             FROM folders f WHERE f.account_id = ?1",
        )?;
        let mut out = st
            .query_map([account_id], |r| {
                Ok(FolderInfo {
                    name: r.get(0)?,
                    raw_name: r.get(1)?,
                    delimiter: r.get(2)?,
                    special: r.get(3)?,
                    unread: r.get(4)?,
                    total: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.sort_by_key(folder_rank);
        Ok(out)
    }

    pub fn folder(&self, account_id: i64, name: &str) -> Result<Option<FolderInfo>> {
        Ok(self.list_folders(account_id)?.into_iter().find(|f| f.name == name))
    }

    pub fn folder_by_special(&self, account_id: i64, special: &str) -> Result<Option<FolderInfo>> {
        Ok(self.list_folders(account_id)?.into_iter().find(|f| f.special.as_deref() == Some(special)))
    }

    pub fn folder_state(&self, account_id: i64, name: &str) -> Result<(u32, u32)> {
        Ok(self
            .conn
            .query_row(
                "SELECT uidvalidity, last_uid FROM folders WHERE account_id = ?1 AND name = ?2",
                params![account_id, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0)))
    }

    /// UIDVALIDITY changed: every cached UID is meaningless now.
    pub fn reset_folder(&self, account_id: i64, name: &str, uidvalidity: u32) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE account_id = ?1 AND folder = ?2", params![account_id, name])?;
        self.conn.execute(
            "UPDATE folders SET uidvalidity = ?3, last_uid = 0 WHERE account_id = ?1 AND name = ?2",
            params![account_id, name, uidvalidity],
        )?;
        self.gc_blobs()?;
        Ok(())
    }

    pub fn set_last_uid(&self, account_id: i64, name: &str, uid: u32) -> Result<()> {
        self.conn.execute("UPDATE folders SET last_uid = ?3 WHERE account_id = ?1 AND name = ?2", params![account_id, name, uid])?;
        Ok(())
    }

    // ---- messages -------------------------------------------------------

    pub fn insert_message(&self, m: &NewMessage) -> Result<i64> {
        if let Some(uid) = m.uid {
            let existing: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE account_id = ?1 AND folder = ?2 AND uid = ?3",
                    params![m.account_id, m.folder, uid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(id);
            }
            // A message we moved locally (uid unknown) showing up in its new folder.
            let moved: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE account_id = ?1 AND folder = ?2 AND message_id = ?3 AND uid IS NULL LIMIT 1",
                    params![m.account_id, m.folder, m.meta.message_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = moved {
                self.conn.execute("UPDATE messages SET uid = ?1 WHERE id = ?2", params![uid, id])?;
                if let Some(raw) = &m.raw {
                    self.set_raw(id, raw)?;
                }
                return Ok(id);
            }
        }

        let tx = self.conn.unchecked_transaction()?;
        let key = self.resolve_thread_key(m.account_id, &m.meta)?;
        let recipients = serde_json::to_string(&RecipientsRef { to: &m.meta.to, cc: &m.meta.cc })?;
        tx.execute(
            "INSERT INTO messages (account_id, folder, uid, pop_uidl, message_id, thread_key, norm_subject, subject,
                from_name, from_email, recipients, date, seen, flagged, has_attachments, size, snippet, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                m.account_id,
                m.folder,
                m.uid,
                m.pop_uidl,
                m.meta.message_id,
                key,
                normalize_subject(&m.meta.subject),
                m.meta.subject,
                m.meta.from.name,
                m.meta.from.email,
                recipients,
                m.meta.date,
                m.seen,
                m.flagged,
                m.meta.has_attachments,
                m.size as i64,
                m.meta.snippet,
                Option::<Vec<u8>>::None,
            ],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut st = tx.prepare_cached("INSERT INTO msg_refs (msg, ref_id) VALUES (?1, ?2)")?;
            let mut seen = HashSet::new();
            for r in m.meta.references.iter().chain(&m.meta.in_reply_to) {
                if seen.insert(r) {
                    st.execute(params![id, r])?;
                }
            }
        }
        if let Some(raw) = &m.raw {
            self.store_raw(id, raw)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Incremental JWZ-style linking: a message joins every thread it is
    /// connected to through parents, children that arrived earlier, or
    /// siblings sharing a (possibly missing) ancestor. If it bridges several
    /// threads they are merged.
    fn resolve_thread_key(&self, account_id: i64, meta: &MessageMeta) -> Result<String> {
        let related: Vec<&String> = meta.references.iter().chain(&meta.in_reply_to).collect();
        let mut keys: Vec<String> = Vec::new();
        {
            let mut by_mid = self
                .conn
                .prepare_cached("SELECT thread_key FROM messages WHERE account_id = ?1 AND message_id = ?2 LIMIT 1")?;
            let mut by_ref = self.conn.prepare_cached(
                "SELECT DISTINCT m.thread_key FROM msg_refs r JOIN messages m ON m.id = r.msg WHERE r.ref_id = ?1 AND m.account_id = ?2",
            )?;
            for id in &related {
                if let Some(k) = by_mid.query_row(params![account_id, id], |r| r.get::<_, String>(0)).optional()? {
                    keys.push(k);
                }
            }
            for id in related.iter().copied().chain(std::iter::once(&meta.message_id)) {
                for k in by_ref.query_map(params![id, account_id], |r| r.get::<_, String>(0))? {
                    keys.push(k?);
                }
            }
            if let Some(k) = by_mid.query_row(params![account_id, meta.message_id], |r| r.get::<_, String>(0)).optional()? {
                keys.push(k); // same message in another folder (e.g. Sent + Inbox)
            }
        }
        if keys.is_empty() && related.is_empty() && is_reply_subject(&meta.subject) {
            let norm = normalize_subject(&meta.subject);
            if !norm.is_empty() {
                let k: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT thread_key FROM messages WHERE account_id = ?1 AND norm_subject = ?2 AND date BETWEEN ?3 AND ?4
                         ORDER BY date DESC LIMIT 1",
                        params![account_id, norm, meta.date - 30 * 86400, meta.date + 86400],
                        |r| r.get(0),
                    )
                    .optional()?;
                keys.extend(k);
            }
        }
        keys.sort();
        keys.dedup();
        let Some(target) = keys.first().cloned() else { return Ok(meta.message_id.clone()) };
        for k in &keys[1..] {
            self.conn.execute(
                "UPDATE messages SET thread_key = ?1 WHERE account_id = ?2 AND thread_key = ?3",
                params![target, account_id, k],
            )?;
        }
        Ok(target)
    }

    /// Conversations in a folder, newest first. `query` uses the search
    /// syntax in [`crate::search`]; free words also match the text of
    /// messages the virus scanner found clean.
    pub fn list_threads(&self, account_id: i64, folder: &str, query: Option<&str>, limit: u32, offset: u32) -> Result<Vec<ThreadSummary>> {
        let q = crate::search::parse(query.unwrap_or(""));
        let mut cond = String::new();
        let mut args: Vec<rusqlite::types::Value> = vec![account_id.into(), folder.to_string().into(), i64::from(limit).into(), i64::from(offset).into()];
        let mut arg = |v: rusqlite::types::Value| {
            args.push(v);
            format!("?{}", args.len())
        };
        let like = |s: &str| rusqlite::types::Value::from(like_pattern(s));
        for w in &q.words {
            let p = arg(like(w));
            cond += &format!(
                " AND (subject LIKE {p} ESCAPE '\\' OR from_name LIKE {p} ESCAPE '\\' OR from_email LIKE {p} ESCAPE '\\' \
                 OR recipients LIKE {p} ESCAPE '\\' OR snippet LIKE {p} ESCAPE '\\' \
                 OR id IN (SELECT msg FROM msg_text WHERE status = 'clean' AND body LIKE {p} ESCAPE '\\'))"
            );
        }
        for f in &q.from {
            let p = arg(like(f));
            cond += &format!(" AND (from_name LIKE {p} ESCAPE '\\' OR from_email LIKE {p} ESCAPE '\\')");
        }
        for t in &q.to {
            let who = if t.eq_ignore_ascii_case("me") {
                // Never an empty pattern (it would match everything).
                match self.get_account(account_id)? {
                    Some(acct) if !acct.email.is_empty() => acct.email,
                    _ => return Ok(Vec::new()),
                }
            } else {
                t.clone()
            };
            let p = arg(like(&who));
            cond += &format!(" AND recipients LIKE {p} ESCAPE '\\'");
        }
        for t in &q.subject {
            let p = arg(like(t));
            cond += &format!(" AND subject LIKE {p} ESCAPE '\\'");
        }
        if q.has_attachment {
            cond += " AND has_attachments = 1";
        }
        if q.unread {
            cond += " AND seen = 0";
        }
        if q.flagged {
            cond += " AND flagged = 1";
        }
        if let Some(t) = q.after {
            cond += &format!(" AND date >= {}", arg(t.into()));
        }
        if let Some(t) = q.before {
            cond += &format!(" AND date < {}", arg(t.into()));
        }
        let sql = format!(
            "SELECT thread_key, MAX(date) AS last, COUNT(*), SUM(seen = 0), MAX(flagged), MAX(has_attachments)
             FROM messages WHERE account_id = ?1 AND folder = ?2 {cond}
             GROUP BY thread_key ORDER BY last DESC LIMIT ?3 OFFSET ?4"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let groups = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, u32>(2)?, r.get::<_, u32>(3)?, r.get::<_, bool>(4)?, r.get::<_, bool>(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut members = self.conn.prepare_cached(
            "SELECT id, subject, from_name, from_email, snippet FROM messages
             WHERE account_id = ?1 AND folder = ?2 AND thread_key = ?3 ORDER BY date DESC",
        )?;
        let mut out = Vec::with_capacity(groups.len());
        for (key, last, count, unread, flagged, att) in groups {
            let rows = members
                .query_map(params![account_id, folder, key], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let Some(latest) = rows.first() else { continue };
            let mut participants: Vec<String> = Vec::new();
            for (_, _, name, email, _) in &rows {
                let p = if name.is_empty() { email.clone() } else { name.clone() };
                if !participants.contains(&p) {
                    participants.push(p);
                }
            }
            // The oldest message carries the original subject.
            let subject = rows.last().map(|r| r.1.clone()).unwrap_or_default();
            out.push(ThreadSummary {
                thread_key: key,
                subject,
                participants,
                snippet: latest.4.clone(),
                last_date: last,
                count,
                unread,
                flagged,
                has_attachments: att,
                latest_id: latest.0,
            });
        }
        Ok(out)
    }

    /// Message-IDs a message replies to, oldest first (its parent last).
    pub fn refs_of(&self, msg: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT ref_id FROM msg_refs WHERE msg = ?1 ORDER BY rowid")?;
        let rows = st.query_map([msg], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Downloaded messages whose text hasn't been scanned for indexing yet.
    pub fn unindexed(&self, limit: u32) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT m.id, m.raw FROM messages m LEFT JOIN msg_text t ON t.msg = m.id
             WHERE m.raw IS NOT NULL AND t.msg IS NULL ORDER BY m.date DESC LIMIT ?1",
        )?;
        let rows = st.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Records a scan result: the text is kept only for clean messages.
    pub fn set_text(&self, msg: i64, clean: bool, body: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO msg_text (msg, status, body) VALUES (?1, ?2, ?3)",
            params![msg, if clean { "clean" } else { "infected" }, clean.then_some(body)],
        )?;
        Ok(())
    }

    /// Every message in a conversation, across folders (so your own replies
    /// from Sent show up inline), oldest first.
    pub fn thread_messages(&self, account_id: i64, thread_key: &str) -> Result<Vec<StoredMessage>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {MSG_COLS} FROM messages WHERE account_id = ?1 AND thread_key = ?2 ORDER BY date ASC, id ASC"
        ))?;
        let rows = st.query_map(params![account_id, thread_key], row_to_msg)?.collect::<rusqlite::Result<Vec<_>>>()?;
        // Drop duplicates of the same message in several folders, preferring
        // the non-Sent copy.
        let mut seen = HashSet::new();
        Ok(rows.into_iter().filter(|m| seen.insert(m.message_id.clone())).collect())
    }

    pub fn messages_in_folder(&self, account_id: i64, folder: &str) -> Result<Vec<StoredMessage>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {MSG_COLS} FROM messages WHERE account_id = ?1 AND folder = ?2 ORDER BY date DESC"
        ))?;
        let rows = st.query_map(params![account_id, folder], row_to_msg)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn message(&self, id: i64) -> Result<Option<StoredMessage>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {MSG_COLS} FROM messages WHERE id = ?1"), [id], row_to_msg)
            .optional()?)
    }

    pub fn messages(&self, ids: &[i64]) -> Result<Vec<StoredMessage>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(m) = self.message(*id)? {
                out.push(m);
            }
        }
        Ok(out)
    }

    pub fn references_of(&self, id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT ref_id FROM msg_refs WHERE msg = ?1 ORDER BY rowid")?;
        let rows = st.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Returns the original message bytes, splicing deduplicated
    /// attachment bodies back in.
    pub fn raw(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let Some(stripped) = self
            .conn
            .query_row("SELECT raw FROM messages WHERE id = ?1", [id], |r| r.get::<_, Option<Vec<u8>>>(0))
            .optional()?
            .flatten()
        else {
            return Ok(None);
        };
        let mut st = self.conn.prepare_cached(
            "SELECT mb.offset, b.data FROM msg_blobs mb JOIN blobs b ON b.hash = mb.hash WHERE mb.msg = ?1 ORDER BY mb.seq",
        )?;
        let parts = st
            .query_map([id], |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if parts.is_empty() {
            return Ok(Some(stripped));
        }
        let mut out = Vec::with_capacity(stripped.len() + parts.iter().map(|p| p.1.len()).sum::<usize>());
        let mut pos = 0;
        for (offset, data) in parts {
            let offset = offset.min(stripped.len()).max(pos);
            out.extend_from_slice(&stripped[pos..offset]);
            out.extend_from_slice(&data);
            pos = offset;
        }
        out.extend_from_slice(&stripped[pos..]);
        Ok(Some(out))
    }

    fn store_raw(&self, id: i64, raw: &[u8]) -> Result<()> {
        let (stripped, parts) = crate::dedup::split_attachments(raw);
        self.conn.execute("DELETE FROM msg_blobs WHERE msg = ?1", [id])?;
        self.conn.execute("UPDATE messages SET raw = ?1 WHERE id = ?2", params![stripped, id])?;
        let mut put = self.conn.prepare_cached("INSERT OR IGNORE INTO blobs (hash, data) VALUES (?1, ?2)")?;
        let mut link = self.conn.prepare_cached("INSERT INTO msg_blobs (msg, seq, offset, hash) VALUES (?1, ?2, ?3, ?4)")?;
        for (seq, p) in parts.iter().enumerate() {
            put.execute(params![p.hash, &raw[p.start..p.end]])?;
            link.execute(params![id, seq as i64, p.offset as i64, p.hash])?;
        }
        Ok(())
    }

    /// Removes attachment blobs no message references any more.
    pub fn gc_blobs(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM blobs WHERE hash NOT IN (SELECT hash FROM msg_blobs)", [])?)
    }

    /// (logical bytes, bytes actually stored) for attachment bodies.
    pub fn attachment_usage(&self) -> Result<(u64, u64)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE((SELECT SUM(LENGTH(b.data)) FROM msg_blobs mb JOIN blobs b ON b.hash = mb.hash), 0),
                    COALESCE((SELECT SUM(LENGTH(data)) FROM blobs), 0)",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
    }

    pub fn set_raw(&self, id: i64, raw: &[u8]) -> Result<()> {
        self.store_raw(id, raw)?;
        let body = parse_body(raw);
        let snippet = body.text.as_deref().map(crate::message::snippet_of).unwrap_or_default();
        self.conn.execute(
            "UPDATE messages SET snippet = CASE WHEN ?1 = '' THEN snippet ELSE ?1 END,
                has_attachments = (has_attachments OR ?2) WHERE id = ?3",
            params![snippet, !body.attachments.is_empty(), id],
        )?;
        Ok(())
    }

    pub fn set_seen(&self, id: i64, seen: bool) -> Result<()> {
        self.conn.execute("UPDATE messages SET seen = ?1 WHERE id = ?2", params![seen, id])?;
        Ok(())
    }

    pub fn set_flagged(&self, id: i64, flagged: bool) -> Result<()> {
        self.conn.execute("UPDATE messages SET flagged = ?1 WHERE id = ?2", params![flagged, id])?;
        Ok(())
    }

    /// Local move. The UID becomes unknown until the target folder syncs.
    pub fn set_folder(&self, id: i64, folder: &str) -> Result<()> {
        self.conn.execute("UPDATE messages SET folder = ?1, uid = NULL WHERE id = ?2", params![folder, id])?;
        Ok(())
    }

    pub fn delete_message(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        self.gc_blobs()?;
        Ok(())
    }

    pub fn uids_in_folder(&self, account_id: i64, folder: &str) -> Result<HashSet<u32>> {
        let mut st = self.conn.prepare_cached("SELECT uid FROM messages WHERE account_id = ?1 AND folder = ?2 AND uid IS NOT NULL")?;
        let rows = st.query_map(params![account_id, folder], |r| r.get::<_, u32>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    /// Messages on the server whose body isn't cached, newest first.
    pub fn messages_missing_body(&self, account_id: i64, folder: &str, since: Option<i64>) -> Result<Vec<StoredMessage>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {MSG_COLS} FROM messages WHERE account_id = ?1 AND folder = ?2 AND raw IS NULL AND uid IS NOT NULL AND date >= ?3
             ORDER BY date DESC"
        ))?;
        let rows = st.query_map(params![account_id, folder, since.unwrap_or(i64::MIN)], row_to_msg)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Frees cached bodies older than `before` (headers stay, bodies download
    /// again when opened). Never touches the Outbox. Returns how many.
    pub fn clear_bodies_before(&self, account_id: i64, before: i64) -> Result<usize> {
        let sel = "SELECT id FROM messages WHERE account_id = ?1 AND date < ?2 AND raw IS NOT NULL AND uid IS NOT NULL";
        self.conn.execute(&format!("DELETE FROM msg_blobs WHERE msg IN ({sel})"), params![account_id, before])?;
        let n = self.conn.execute(
            "UPDATE messages SET raw = NULL WHERE account_id = ?1 AND date < ?2 AND raw IS NOT NULL AND uid IS NOT NULL",
            params![account_id, before],
        )?;
        self.gc_blobs()?;
        Ok(n)
    }

    pub fn known_uidls(&self, account_id: i64) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT pop_uidl FROM messages WHERE account_id = ?1 AND pop_uidl IS NOT NULL")?;
        let rows = st.query_map([account_id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(rows)
    }

    // ---- outbox ---------------------------------------------------------

    pub fn queue_outgoing(&self, account_id: i64, msg: i64, rcpts: &[String]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO outbox (account_id, msg, rcpts) VALUES (?1, ?2, ?3)",
            params![account_id, msg, serde_json::to_string(rcpts)?],
        )?;
        Ok(())
    }

    /// (outbox id, message id, recipients) for an account, oldest first.
    pub fn outbox(&self, account_id: i64) -> Result<Vec<(i64, i64, Vec<String>)>> {
        let mut st = self.conn.prepare("SELECT id, msg, rcpts FROM outbox WHERE account_id = ?1 ORDER BY id")?;
        let rows = st
            .query_map([account_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(id, msg, j)| (id, msg, serde_json::from_str(&j).unwrap_or_default())).collect())
    }

    pub fn outbox_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute("UPDATE outbox SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1", params![id, error])?;
        Ok(())
    }

    pub fn outbox_done(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- calendars --------------------------------------------------------

    /// The always-present offline calendar.
    pub fn ensure_local_calendar(&self) -> Result<i64> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT c.id FROM calendars c JOIN cal_sources s ON s.id = c.source_id WHERE s.kind = 'local' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let src = self.add_cal_source("local", None, "", "", "On this device")?;
        self.upsert_calendar(src, "local", "My calendar", Some("#0f6cbd"), false)
    }

    pub fn add_cal_source(&self, kind: &str, account_id: Option<i64>, url: &str, username: &str, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO cal_sources (kind, account_id, url, username, name) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, account_id, url, username, name],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn cal_sources(&self) -> Result<Vec<CalSource>> {
        let mut st = self.conn.prepare("SELECT id, kind, account_id, url, username, name FROM cal_sources ORDER BY id")?;
        let rows = st
            .query_map([], |r| {
                Ok(CalSource { id: r.get(0)?, kind: r.get(1)?, account_id: r.get(2)?, url: r.get(3)?, username: r.get(4)?, name: r.get(5)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn cal_source(&self, id: i64) -> Result<Option<CalSource>> {
        Ok(self.cal_sources()?.into_iter().find(|s| s.id == id))
    }

    pub fn delete_cal_source(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM cal_sources WHERE id = ?1 AND kind != 'local'", [id])?;
        Ok(())
    }

    pub fn upsert_calendar(&self, source_id: i64, remote_id: &str, name: &str, color: Option<&str>, read_only: bool) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO calendars (source_id, remote_id, name, color, read_only) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_id, remote_id) DO UPDATE SET name = excluded.name, color = COALESCE(excluded.color, color), read_only = excluded.read_only",
            params![source_id, remote_id, name, color, read_only],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM calendars WHERE source_id = ?1 AND remote_id = ?2",
            params![source_id, remote_id],
            |r| r.get(0),
        )?)
    }

    /// Removes calendars that disappeared from the server.
    pub fn retain_calendars(&self, source_id: i64, keep: &[String]) -> Result<()> {
        let ids: Vec<(i64, String)> = {
            let mut st = self.conn.prepare("SELECT id, remote_id FROM calendars WHERE source_id = ?1")?;
            let rows = st.query_map([source_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, remote) in ids {
            if !keep.contains(&remote) {
                self.conn.execute("DELETE FROM calendars WHERE id = ?1", [id])?;
            }
        }
        Ok(())
    }

    pub fn calendars(&self) -> Result<Vec<CalendarInfo>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, c.source_id, s.kind, s.name, c.remote_id, c.name, c.color, c.read_only, c.visible
             FROM calendars c JOIN cal_sources s ON s.id = c.source_id ORDER BY s.id, c.name",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(CalendarInfo {
                    id: r.get(0)?,
                    source_id: r.get(1)?,
                    source_kind: r.get(2)?,
                    source_name: r.get(3)?,
                    remote_id: r.get(4)?,
                    name: r.get(5)?,
                    color: r.get(6)?,
                    read_only: r.get(7)?,
                    visible: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn calendar(&self, id: i64) -> Result<Option<CalendarInfo>> {
        Ok(self.calendars()?.into_iter().find(|c| c.id == id))
    }

/// Deletes an extra local calendar (the default "My calendar" stays).
    pub fn delete_local_calendar(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM calendars WHERE id = ?1 AND remote_id != 'local'
               AND source_id IN (SELECT id FROM cal_sources WHERE kind = 'local')",
            [id],
        )?;
        Ok(())
    }

    pub fn set_calendar_visible(&self, id: i64, visible: bool) -> Result<()> {
        self.conn.execute("UPDATE calendars SET visible = ?1 WHERE id = ?2", params![visible, id])?;
        Ok(())
    }

    /// Replaces a calendar's events overlapping [from, to) with a fresh copy.
    pub fn replace_events(&self, calendar_id: i64, from: i64, to: i64, events: &[(String, Option<String>, crate::ical::Event)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM events WHERE calendar_id = ?1 AND end > ?2 AND start < ?3", params![calendar_id, from, to])?;
        for (remote_id, etag, e) in events {
            self.insert_event(calendar_id, remote_id, etag.as_deref(), e)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn insert_event(&self, calendar_id: i64, remote_id: &str, etag: Option<&str>, e: &crate::ical::Event) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (calendar_id, remote_id, etag, uid, title, location, description, start, end, all_day, floating, recurring, tzid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![calendar_id, remote_id, etag, e.uid, e.title, e.location, e.description, e.start, e.end, e.all_day, e.floating, e.recurring, e.tzid],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_event(&self, id: i64, remote_id: &str, etag: Option<&str>, e: &crate::ical::Event) -> Result<()> {
        self.conn.execute(
            "UPDATE events SET remote_id = ?2, etag = ?3, uid = ?4, title = ?5, location = ?6, description = ?7, start = ?8, end = ?9,
                all_day = ?10, floating = ?11, recurring = ?12, tzid = ?13 WHERE id = ?1",
            params![id, remote_id, etag, e.uid, e.title, e.location, e.description, e.start, e.end, e.all_day, e.floating, e.recurring, e.tzid],
        )?;
        Ok(())
    }

    pub fn delete_event(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM events WHERE id = ?1", [id])?;
        Ok(())
    }

    fn row_to_event(r: &Row) -> rusqlite::Result<StoredEvent> {
        Ok(StoredEvent {
            id: r.get(0)?,
            calendar_id: r.get(1)?,
            remote_id: r.get(2)?,
            etag: r.get(3)?,
            event: crate::ical::Event {
                uid: r.get(4)?,
                title: r.get(5)?,
                location: r.get(6)?,
                description: r.get(7)?,
                start: r.get(8)?,
                end: r.get(9)?,
                all_day: r.get(10)?,
                floating: r.get(11)?,
                tzid: r.get(13)?,
                tz_offset: None,
                recurring: r.get(12)?,
            },
        })
    }

    const EVENT_COLS: &'static str =
        "e.id, e.calendar_id, e.remote_id, e.etag, e.uid, e.title, e.location, e.description, e.start, e.end, e.all_day, e.floating, e.recurring, e.tzid";

    /// Events of visible calendars overlapping [from, to), by start time.
    pub fn events_between(&self, from: i64, to: i64) -> Result<Vec<StoredEvent>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {} FROM events e JOIN calendars c ON c.id = e.calendar_id
             WHERE c.visible = 1 AND e.end > ?1 AND e.start < ?2 ORDER BY e.start, e.end",
            Self::EVENT_COLS
        ))?;
        let rows = st.query_map(params![from, to], Self::row_to_event)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Events in visible calendars whose title, location or description
    /// contain every word of `query`, by date (at most `limit`).
    pub fn search_events(&self, query: &str, limit: u32) -> Result<Vec<StoredEvent>> {
        let words: Vec<String> = query.split_whitespace().take(8).map(like_pattern).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let cond: String = (0..words.len())
            .map(|i| format!(" AND (e.title LIKE ?{n} ESCAPE '\\' OR e.location LIKE ?{n} ESCAPE '\\' OR e.description LIKE ?{n} ESCAPE '\\')", n = i + 2))
            .collect();
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM events e JOIN calendars c ON c.id = e.calendar_id WHERE c.visible = 1{cond} ORDER BY e.start LIMIT ?1",
            Self::EVENT_COLS
        ))?;
        let mut args: Vec<rusqlite::types::Value> = vec![i64::from(limit).into()];
        args.extend(words.into_iter().map(Into::into));
        let rows = st.query_map(rusqlite::params_from_iter(args.iter()), Self::row_to_event)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn event(&self, id: i64) -> Result<Option<StoredEvent>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {} FROM events e WHERE e.id = ?1", Self::EVENT_COLS), [id], Self::row_to_event)
            .optional()?)
    }

    // ---- offline action queue ------------------------------------------

    pub fn queue_op(&self, account_id: i64, data: &str) -> Result<()> {
        self.conn.execute("INSERT INTO pending_ops (account_id, data) VALUES (?1, ?2)", params![account_id, data])?;
        Ok(())
    }

    pub fn pending_ops(&self, account_id: i64) -> Result<Vec<(i64, String)>> {
        let mut st = self.conn.prepare("SELECT id, data FROM pending_ops WHERE account_id = ?1 ORDER BY id")?;
        let rows = st.query_map([account_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_op(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- rules ----------------------------------------------------------

    pub fn list_rules(&self) -> Result<Vec<Rule>> {
        let mut st = self.conn.prepare("SELECT id, data FROM rules ORDER BY position, id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data) = row?;
            let mut rule: Rule = serde_json::from_str(&data)?;
            rule.id = Some(id);
            out.push(rule);
        }
        Ok(out)
    }

    pub fn rules_for(&self, account_id: i64) -> Result<Vec<Rule>> {
        Ok(self.list_rules()?.into_iter().filter(|r| r.applies_to_account(account_id)).collect())
    }

    pub fn save_rule(&self, rule: &mut Rule) -> Result<()> {
        rule.validate().map_err(Error::Other)?;
        match rule.id {
            Some(id) => {
                self.conn.execute("UPDATE rules SET data = ?1 WHERE id = ?2", params![serde_json::to_string(rule)?, id])?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO rules (position, data) VALUES ((SELECT COALESCE(MAX(position), 0) + 1 FROM rules), '{}')",
                    [],
                )?;
                rule.id = Some(self.conn.last_insert_rowid());
                self.conn.execute(
                    "UPDATE rules SET data = ?1 WHERE id = ?2",
                    params![serde_json::to_string(rule)?, rule.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM rules WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn reorder_rules(&self, ids: &[i64]) -> Result<()> {
        for (pos, id) in ids.iter().enumerate() {
            self.conn.execute("UPDATE rules SET position = ?1 WHERE id = ?2", params![pos as i64, id])?;
        }
        Ok(())
    }

    // ---- secrets (only used where no OS keystore is available) ----------

    pub fn get_secret(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM secrets WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO secrets (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_secret(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM secrets WHERE key = ?1", [key])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Protocol;
    use crate::net::{Endpoint, Security};

    fn store_with_account() -> (Store, i64) {
        let s = Store::open_in_memory().unwrap();
        let ep = Endpoint { host: "h".into(), port: 993, security: Security::Tls, allow_insecure: false };
        let mut a = Account {
            id: 0,
            name: "Me".into(),
            email: "me@x.org".into(),
            username: "me".into(),
            incoming_protocol: Protocol::Imap,
            incoming: ep.clone(),
            smtp: ep,
            smtp_username: None,
            signature: String::new(),
            auth: Default::default(),
        };
        s.add_account(&mut a).unwrap();
        s.upsert_folder(a.id, "INBOX", "INBOX", Some("/"), Some("inbox")).unwrap();
        (s, a.id)
    }

    fn meta(id: &str, refs: &[&str], subject: &str, date: i64) -> MessageMeta {
        MessageMeta {
            message_id: id.into(),
            references: refs.iter().map(|s| s.to_string()).collect(),
            in_reply_to: refs.last().map(|s| vec![s.to_string()]).unwrap_or_default(),
            subject: subject.into(),
            from: Addr { name: format!("Sender {id}"), email: format!("{id}@x.org") },
            date,
            ..Default::default()
        }
    }

    fn insert(s: &Store, acct: i64, uid: u32, m: MessageMeta) -> i64 {
        s.insert_message(&NewMessage {
            account_id: acct,
            folder: "INBOX".into(),
            uid: Some(uid),
            pop_uidl: None,
            meta: m,
            seen: false,
            flagged: false,
            size: 10,
            raw: None,
        })
        .unwrap()
    }

    fn key_of(s: &Store, id: i64) -> String {
        s.message(id).unwrap().unwrap().thread_key
    }

    #[test]
    fn threads_by_references_in_any_order() {
        let (s, a) = store_with_account();
        // Child arrives before its parent.
        let c = insert(&s, a, 2, meta("c", &["root", "p"], "Re: Topic", 200));
        let p = insert(&s, a, 1, meta("p", &["root"], "Re: Topic", 100));
        let root = insert(&s, a, 3, meta("root", &[], "Topic", 50));
        assert_eq!(key_of(&s, c), key_of(&s, p));
        assert_eq!(key_of(&s, p), key_of(&s, root));
        let other = insert(&s, a, 4, meta("z", &[], "Unrelated", 300));
        assert_ne!(key_of(&s, other), key_of(&s, root));

        let threads = s.list_threads(a, "INBOX", None, 50, 0).unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].subject, "Unrelated");
        assert_eq!(threads[1].count, 3);
        assert_eq!(threads[1].subject, "Topic");
        assert_eq!(threads[1].unread, 3);
    }

    #[test]
    fn siblings_with_missing_parent_join() {
        let (s, a) = store_with_account();
        let x = insert(&s, a, 1, meta("x", &["missing"], "Re: Q", 100));
        let y = insert(&s, a, 2, meta("y", &["missing"], "Re: Q", 200));
        assert_eq!(key_of(&s, x), key_of(&s, y));
    }

    #[test]
    fn bridging_message_merges_threads() {
        let (s, a) = store_with_account();
        let x = insert(&s, a, 1, meta("x", &[], "A", 100));
        let y = insert(&s, a, 2, meta("y", &[], "B", 200));
        assert_ne!(key_of(&s, x), key_of(&s, y));
        let z = insert(&s, a, 3, meta("z", &["x", "y"], "Re: A", 300));
        assert_eq!(key_of(&s, x), key_of(&s, y));
        assert_eq!(key_of(&s, z), key_of(&s, x));
    }

    #[test]
    fn subject_fallback_only_for_replies() {
        let (s, a) = store_with_account();
        let x = insert(&s, a, 1, meta("x", &[], "Lunch?", 1000));
        let y = insert(&s, a, 2, meta("y", &[], "RE: Lunch?", 2000));
        let z = insert(&s, a, 3, meta("z", &[], "Lunch?", 3000));
        assert_eq!(key_of(&s, x), key_of(&s, y));
        assert_ne!(key_of(&s, z), key_of(&s, x));
    }

    #[test]
    fn duplicate_uid_is_ignored_and_moved_message_reconciles() {
        let (s, a) = store_with_account();
        s.upsert_folder(a, "Archive", "Archive", Some("/"), Some("archive")).unwrap();
        let id = insert(&s, a, 1, meta("m", &[], "Hi", 1));
        assert_eq!(insert(&s, a, 1, meta("m", &[], "Hi", 1)), id);
        s.set_folder(id, "Archive").unwrap();
        let again = s
            .insert_message(&NewMessage {
                account_id: a,
                folder: "Archive".into(),
                uid: Some(77),
                pop_uidl: None,
                meta: meta("m", &[], "Hi", 1),
                seen: true,
                flagged: false,
                size: 1,
                raw: None,
            })
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(s.message(id).unwrap().unwrap().uid, Some(77));
    }

    #[test]
    fn searches_event_contents() {
        let (s, _) = store_with_account();
        let cal = s.ensure_local_calendar().unwrap();
        let ev = |title: &str, location: &str, description: &str, start: i64| crate::ical::Event {
            title: title.into(), location: location.into(), description: description.into(), start, end: start + 3600, ..Default::default()
        };
        s.insert_event(cal, "a", None, &ev("Dentist", "Rua Augusta 10", "", 300)).unwrap();
        s.insert_event(cal, "b", None, &ev("Team sync", "Room 4", "Agenda: budget review and 100% roadmap", 100)).unwrap();
        s.insert_event(cal, "c", None, &ev("Lunch", "Cantina", "budget-friendly", 200)).unwrap();
        let titles = |q: &str| s.search_events(q, 50).unwrap().into_iter().map(|e| e.event.title).collect::<Vec<_>>();
        assert_eq!(titles("budget"), ["Team sync", "Lunch"], "description, by date");
        assert_eq!(titles("augusta"), ["Dentist"], "location, any case");
        assert_eq!(titles("budget review"), ["Team sync"], "every word must match");
        assert_eq!(titles("100%"), ["Team sync"], "LIKE wildcards are literal");
        assert!(titles("   ").is_empty());
    }

    #[test]
    fn search_operators_and_clean_only_content() {
        let (s, a) = store_with_account();
        let mut m1 = meta("m1", &[], "Q1 roadmap", 1_790_900_000); // Oct 2026
        m1.to = vec![Addr { name: "Me".into(), email: "me@x.org".into() }];
        m1.has_attachments = true;
        let id1 = insert(&s, a, 1, m1);
        let mut m2 = meta("m2", &[], "Lunch", 1_793_600_000); // Nov 2026
        m2.to = vec![Addr { name: "Carol".into(), email: "carol@x.org".into() }];
        let id2 = insert(&s, a, 2, m2);
        let n = |q: &str| s.list_threads(a, "INBOX", Some(q), 50, 0).unwrap().len();
        assert_eq!(n("from:m1"), 1);
        assert_eq!(n("from:\"Sender m2\""), 1);
        assert_eq!(n("to:me"), 1);
        assert_eq!(n("to:carol"), 1);
        assert_eq!(n("subject:roadmap"), 1);
        assert_eq!(n("has:attachment"), 1);
        assert_eq!(n("is:unread"), 2);
        assert_eq!(n("is:flagged"), 0);
        assert_eq!(n("after:2026-11-01"), 1);
        assert_eq!(n("before:2026-11-01 has:attachment"), 1);
        assert_eq!(n("from:m1 to:carol"), 0);
        // Body text is searchable only for messages the scanner found clean.
        s.set_text(id1, true, "the secret budget figures").unwrap();
        s.set_text(id2, false, "budget in an infected message").unwrap();
        assert_eq!(n("budget"), 1);
        assert_eq!(n("infected"), 0, "infected text is never stored");
        assert!(s.unindexed(10).unwrap().is_empty() || s.unindexed(10).unwrap().iter().all(|(id, _)| *id != id1));
    }

    #[test]
    fn search_and_folders() {
        let (s, a) = store_with_account();
        insert(&s, a, 1, meta("m1", &[], "Invoice 100%", 1));
        insert(&s, a, 2, meta("m2", &[], "Party", 2));
        assert_eq!(s.list_threads(a, "INBOX", Some("100%"), 50, 0).unwrap().len(), 1);
        assert_eq!(s.list_threads(a, "INBOX", Some("sender m2"), 50, 0).unwrap().len(), 1);
        let f = s.list_folders(a).unwrap();
        assert_eq!(f[0].name, "INBOX");
        assert_eq!(f[0].unread, 2);
    }

    #[test]
    fn attachments_are_deduplicated_and_restored_exactly() {
        let (s, a) = store_with_account();
        let payload = "QUJD".repeat(4000); // 16 KB of base64
        let mk = |mid: &str| {
            format!(
                "Message-ID: <{mid}>\r\nSubject: report\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--b\r\nContent-Type: application/pdf; name=\"r.pdf\"\r\nContent-Disposition: attachment; filename=\"r.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{payload}\r\n--b--\r\n"
            )
            .into_bytes()
        };
        let (r1, r2) = (mk("one"), mk("two"));
        let id1 = insert(&s, a, 1, meta("one", &[], "report", 1));
        let id2 = insert(&s, a, 2, meta("two", &[], "report", 2));
        s.set_raw(id1, &r1).unwrap();
        s.set_raw(id2, &r2).unwrap();
        assert_eq!(s.raw(id1).unwrap().unwrap(), r1);
        assert_eq!(s.raw(id2).unwrap().unwrap(), r2);
        let (logical, stored) = s.attachment_usage().unwrap();
        assert!(logical >= 2 * 16000 && stored * 2 == logical, "{logical} {stored}");
        s.delete_message(id1).unwrap();
        assert_eq!(s.raw(id2).unwrap().unwrap(), r2);
        s.delete_message(id2).unwrap();
        assert_eq!(s.attachment_usage().unwrap(), (0, 0));
    }

    #[test]
    fn calendar_storage() {
        let s = Store::open_in_memory().unwrap();
        let local = s.ensure_local_calendar().unwrap();
        assert_eq!(s.ensure_local_calendar().unwrap(), local);
        let src = s.add_cal_source("caldav", None, "https://dav.x/", "me", "Work").unwrap();
        let work = s.upsert_calendar(src, "https://dav.x/cal/work/", "Work", None, false).unwrap();
        assert_eq!(s.upsert_calendar(src, "https://dav.x/cal/work/", "Work!", Some("#ff0000"), false).unwrap(), work);
        let ev = |t: &str, start: i64| crate::ical::Event { title: t.into(), start, end: start + 3600, ..Default::default() };
        s.insert_event(local, "", None, &ev("dentist", 1000)).unwrap();
        s.replace_events(work, 0, 100_000, &[("h1".into(), None, ev("old", 5000))]).unwrap();
        s.replace_events(work, 0, 100_000, &[("h2".into(), Some("e".into()), ev("new", 6000))]).unwrap();
        let all = s.events_between(0, 100_000).unwrap();
        assert_eq!(all.iter().map(|e| e.event.title.as_str()).collect::<Vec<_>>(), ["dentist", "new"]);
        s.set_calendar_visible(work, false).unwrap();
        assert_eq!(s.events_between(0, 100_000).unwrap().len(), 1);
        s.retain_calendars(src, &[]).unwrap();
        assert_eq!(s.calendars().unwrap().len(), 1);
        s.delete_cal_source(src).unwrap();
        s.delete_cal_source(s.calendar(local).unwrap().unwrap().source_id).unwrap(); // local can't be removed
        assert_eq!(s.calendars().unwrap()[0].name, "My calendar");
    }

    #[test]
    fn rules_crud_and_order() {
        let (s, _) = store_with_account();
        let mut r1 = Rule::move_from_sender("a@x", "A");
        let mut r2 = Rule::move_from_sender("b@x", "B");
        s.save_rule(&mut r1).unwrap();
        s.save_rule(&mut r2).unwrap();
        assert_eq!(s.list_rules().unwrap().iter().map(|r| r.name.clone()).collect::<Vec<_>>(), vec![r1.name.clone(), r2.name.clone()]);
        s.reorder_rules(&[r2.id.unwrap(), r1.id.unwrap()]).unwrap();
        assert_eq!(s.list_rules().unwrap()[0].id, r2.id);
        s.delete_rule(r2.id.unwrap()).unwrap();
        assert_eq!(s.list_rules().unwrap().len(), 1);
        let mut bad = Rule::move_from_sender("x", "");
        assert!(s.save_rule(&mut bad).is_err());
    }
}
