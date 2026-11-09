//! User actions: open, move, flag, delete, send, "run rules now".
//! Server first, then the local cache, so the two never disagree.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::account::{Account, Credential, Protocol};
use crate::error::{Error, Result};
use crate::imap::ImapClient;
use crate::net::MailStream;
use crate::message::{build_message, parse_address_list, parse_body, parse_meta, split_quoted, Addr, AttachmentInfo, Outgoing};
use crate::rules::evaluate;
use crate::scan::{self, ScannerConfig, Verdict};
use crate::store::StoredMessage;
use crate::sync::{ensure_imap_folder, imap_session, lock, rule_destination};
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flag {
    Seen,
    Flagged,
}

impl Flag {
    fn imap(self) -> &'static str {
        match self {
            Flag::Seen => "\\Seen",
            Flag::Flagged => "\\Flagged",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    #[serde(flatten)]
    pub msg: StoredMessage,
    pub body_text: String,
    pub quoted_text: String,
    /// Only present when the scan didn't flag the message.
    pub body_html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
    /// Attachments are listed but may not be opened (failed or missing scan).
    pub attachments_blocked: bool,
    pub verdict: Verdict,
    /// Heuristic findings (deceptive links, dangerous attachments).
    pub warnings: Vec<String>,
    /// How many levels of the quoted history (parent first) are messages
    /// missing from this conversation, so worth showing; 0 when the replied-to
    /// message is right here. u32::MAX: unknown (a quote without references).
    pub quote_missing: u32,
    /// Remote images in the HTML (none load unless the user asks; tracker
    /// hosts never do).
    pub remote_images: Vec<crate::tracking::RemoteImage>,
    /// Calendar invitations it carried, as now in the Invitations calendar.
    pub invites: Vec<crate::store::StoredEvent>,
}

/// After a clean scan: the message's text becomes searchable and its
/// invitations go into the calendar.
fn on_clean(store: &Mutex<Store>, id: i64, raw: &[u8]) -> Result<()> {
    if !lock(store)?.is_indexed(id)? {
        let text = search_text(store, raw);
        lock(store)?.set_text(id, true, &text)?;
    }
    let mut st = lock(store)?;
    if let Some(m) = st.message(id)? {
        if crate::invites::import(&mut st, id, &m.from.email, raw, crate::calendar::now()).is_ok_and(|t| !t.is_empty()) {
            INVITES_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(())
}

static INVITES_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether invitations changed the calendar since the last call (so an open
/// calendar can refresh).
pub fn take_invites_changed() -> bool {
    INVITES_CHANGED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Leading levels of `refs` (parent first) not among `present`.
pub fn missing_quote_levels(refs: &[String], present: &HashSet<&str>, has_quote: bool) -> u32 {
    if refs.is_empty() {
        return if has_quote { u32::MAX } else { 0 };
    }
    refs.iter().rev().take_while(|r| !present.contains(r.as_str())).count() as u32
}

#[derive(Debug, Clone, Deserialize)]
pub struct Draft {
    pub to: String,
    #[serde(default)]
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub reply_to_message: Option<i64>,
}

/// Groups messages by folder, keeping only those with a known UID.
fn by_folder(msgs: &[StoredMessage]) -> BTreeMap<String, Vec<&StoredMessage>> {
    let mut map: BTreeMap<String, Vec<&StoredMessage>> = BTreeMap::new();
    for m in msgs {
        map.entry(m.folder.clone()).or_default().push(m);
    }
    map
}

fn uid_set(msgs: &[&StoredMessage]) -> Option<String> {
    let uids: Vec<String> = msgs.iter().filter_map(|m| m.uid).map(|u| u.to_string()).collect();
    if uids.is_empty() { None } else { Some(uids.join(",")) }
}

fn folder_raw(store: &Mutex<Store>, account_id: i64, name: &str) -> Result<String> {
    Ok(lock(store)?.folder(account_id, name)?.map(|f| f.raw_name).unwrap_or_else(|| name.to_string()))
}

/// Loads a conversation, downloading any bodies not cached yet, and scans
/// each message before exposing HTML.
pub async fn open_thread(store: &Mutex<Store>, account: &Account, password: &Credential, thread_key: &str, scanner: &ScannerConfig) -> Result<Vec<MessageView>> {
    let msgs = lock(store)?.thread_messages(account.id, thread_key)?;
    let missing: Vec<StoredMessage> = msgs.iter().filter(|m| !m.has_raw && m.uid.is_some()).cloned().collect();
    if !missing.is_empty() && account.incoming_protocol == Protocol::Imap {
        // Offline (or server down): show what is cached instead of failing.
        let _ = fetch_bodies(store, account, password, &missing).await;
    }
    build_views(store, msgs, scanner).await
}

async fn fetch_bodies(store: &Mutex<Store>, account: &Account, password: &Credential, missing: &[StoredMessage]) -> Result<()> {
    {
        let mut c = imap_session(account, password).await?;
        for (folder, group) in by_folder(missing) {
            let Some(set) = uid_set(&group) else { continue };
            c.select(&folder_raw(store, account.id, &folder)?).await?;
            for f in c.fetch_full(&set).await? {
                if let (Some(body), Some(m)) = (f.body, group.iter().find(|m| m.uid == Some(f.uid))) {
                    lock(store)?.set_raw(m.id, &body)?;
                }
            }
        }
        c.logout().await?;
    }
    Ok(())
}

/// Of the messages a search matched in one conversation (`hits`, newest
/// first), those where the match is their own: not only in text they quote,
/// and not only in a file an earlier message of the conversation already
/// carried (same SHA-256). So a reply quoting the original, or forwarding
/// its attachment, isn't marked; the original is. Messages whose content
/// isn't on the device are kept as they are; if nothing would remain, the
/// matches are returned unchanged.
pub fn own_hits(st: &Store, account_id: i64, thread_key: &str, hits: &[i64], query: &str) -> Result<Vec<i64>> {
    let q = crate::search::parse(query);
    if q.words.is_empty() && q.attachment_types.is_empty() {
        return Ok(hits.to_vec());
    }
    // Every message of the conversation (any folder), oldest first, with its
    // files as (SHA-256, name).
    type Files = Vec<(String, String)>;
    let mut all: Vec<(i64, i64, Files)> = Vec::new();
    for id in st.thread_message_ids(account_id, thread_key, None)? {
        let Some(m) = st.message(id)? else { continue };
        let files = st
            .raw(id)?
            .map(|raw| crate::message::all_attachments(&raw).into_iter().map(|(name, b)| (crate::update::sha256_hex(&b), name)).collect())
            .unwrap_or_default();
        all.push((id, m.date, files));
    }
    all.sort_by_key(|(id, date, _)| (*date, *id));
    let words: Vec<String> = q.words.iter().map(|w| w.to_lowercase()).collect();
    let mut kept = Vec::new();
    for &id in hits {
        let Some(raw) = st.raw(id)? else {
            kept.push(id);
            continue;
        };
        let Some(m) = st.message(id)? else { continue };
        let pos = all.iter().position(|(x, _, _)| *x == id).unwrap_or(0);
        let earlier: HashSet<&str> = all[..pos].iter().flat_map(|(_, _, f)| f.iter().map(|(h, _)| h.as_str())).collect();
        let own_files: Vec<&(String, String)> = all.get(pos).map(|(_, _, f)| f.iter().filter(|(h, _)| !earlier.contains(h.as_str())).collect()).unwrap_or_default();
        let body = parse_body(&raw);
        let text = body.text.or_else(|| body.html.as_deref().map(crate::safety::strip_tags)).unwrap_or_default();
        let (new, _quoted) = split_quoted(&text);
        let mut own = format!("{} {} {} {}", m.subject, m.from.name, m.from.email, new).to_lowercase();
        for (hash, name) in &own_files {
            own.push(' ');
            own.push_str(&name.to_lowercase());
            if let Some(t) = st.attachment_text(hash)? {
                own.push(' ');
                own.push_str(&t.to_lowercase());
            }
        }
        let words_ok = words.iter().all(|w| own.contains(w.as_str()));
        let types_ok = q.attachment_types.iter().all(|exts| {
            own_files.iter().any(|(_, name)| name.rsplit_once('.').is_some_and(|(_, e)| exts.iter().any(|x| x.eq_ignore_ascii_case(e))))
        });
        if words_ok && types_ok {
            kept.push(id);
        }
    }
    Ok(if kept.is_empty() { hits.to_vec() } else { kept })
}

/// Searchable text of a message: the plain-text part (or the HTML's text),
/// then each attachment's name and the text inside it (PDF, Office…). A
/// file's text is read once and kept by its SHA-256, so the same file in
/// other messages isn't read again. Only for messages ClamAV found clean.
fn search_text(store: &Mutex<Store>, raw: &[u8]) -> String {
    let body = parse_body(raw);
    let text = body.text.or_else(|| body.html.as_deref().map(crate::safety::strip_tags)).unwrap_or_default();
    // A generous cap keeps the index small (long threads repeat their history).
    let mut out: String = text.chars().take(64 * 1024).collect();
    let files = crate::message::all_attachments(raw);
    for (a, (_, bytes)) in body.attachments.iter().zip(&files) {
        out.push('\n');
        out.push_str(&a.name);
        if a.danger.is_some() || a.size > crate::doctext::MAX_FILE {
            continue;
        }
        let hash = crate::update::sha256_hex(bytes);
        let known = lock(store).ok().and_then(|st| st.attachment_text(&hash).ok().flatten());
        let inside = match known {
            Some(t) => t,
            None => {
                let t = crate::doctext::extract(&a.name, &a.mime, bytes).unwrap_or_default();
                if let Ok(st) = lock(store) {
                    let _ = st.set_attachment_text(&hash, &t);
                }
                t
            }
        };
        if !inside.is_empty() {
            out.push(' ');
            out.push_str(&inside);
        }
    }
    out
}

/// Makes downloaded messages searchable by content, but only those the
/// virus scanner finds clean; infected ones are recorded and never indexed.
/// Stops when the scanner isn't available (retried on the next sync).
/// Returns how many messages were processed.
pub async fn index_clean_text(store: &Mutex<Store>, scanner: &ScannerConfig) -> Result<usize> {
    index_clean_text_with(store, scanner, &|_, _| {}).await
}

/// `index_clean_text`, telling `progress(done, left)` before each batch.
pub async fn index_clean_text_with(store: &Mutex<Store>, scanner: &ScannerConfig, progress: &(dyn Fn(usize, usize) + Send + Sync)) -> Result<usize> {
    let mut done = 0;
    loop {
        let left = lock(store)?.count_unindexed()?;
        let batch = lock(store)?.unindexed(25)?;
        if batch.is_empty() {
            return Ok(done);
        }
        progress(done, left);
        for (id, raw) in batch {
            match scan::scan_cached(store, scanner, &raw).await {
                Verdict::Clean => on_clean(store, id, &raw)?,
                Verdict::Infected { .. } => lock(store)?.set_text(id, false, "")?,
                Verdict::NotScanned { .. } => return Ok(done),
            }
            done += 1;
        }
    }
}

async fn build_views(store: &Mutex<Store>, msgs: Vec<StoredMessage>, scanner: &ScannerConfig) -> Result<Vec<MessageView>> {
    let mut views = Vec::with_capacity(msgs.len());
    let ids: Vec<String> = msgs.iter().map(|m| m.message_id.clone()).collect();
    let present: HashSet<&str> = ids.iter().map(String::as_str).collect();
    for m in msgs {
        let refs = lock(store)?.refs_of(m.id)?;
        let raw = lock(store)?.raw(m.id)?;
        let view = match raw {
            Some(raw) => {
                let verdict = scan::scan_cached(store, scanner, &raw).await;
                match &verdict {
                    Verdict::Clean => on_clean(store, m.id, &raw)?,
                    Verdict::Infected { .. } => lock(store)?.set_text(m.id, false, "")?,
                    Verdict::NotScanned { .. } => {}
                }
                let blocked = match &verdict {
                    Verdict::Infected { .. } => true,
                    Verdict::NotScanned { .. } => scanner.enabled && scanner.block_when_unavailable,
                    Verdict::Clean => false,
                };
                let body = parse_body(&raw);
                let text = body.text.unwrap_or_default();
                let (new, quoted) = split_quoted(&text);
                let mut warnings = body.html.as_deref().map(crate::safety::link_warnings).unwrap_or_default();
                for a in body.attachments.iter().filter(|a| a.danger.is_some()) {
                    warnings.push(format!("Blocked attachment \"{}\": {}.", a.name, a.danger.as_deref().unwrap_or("")));
                }
                let invites = lock(store)?.invites_of(m.id, crate::calendar::now())?;
                let has_quote = !quoted.is_empty() || body.html.as_deref().is_some_and(|h| h.contains("<blockquote") || h.contains("gmail_quote") || h.contains("divRplyFwdMsg"));
                MessageView {
                    msg: m,
                    body_text: new,
                    quote_missing: missing_quote_levels(&refs, &present, has_quote),
                    remote_images: if blocked { Vec::new() } else { body.html.as_deref().map(crate::tracking::remote_images).unwrap_or_default() },
                    quoted_text: quoted,
                    body_html: if blocked { None } else { body.html.as_deref().map(crate::safety::neutralize_html) },
                    attachments: body.attachments,
                    attachments_blocked: blocked,
                    invites,
                    verdict,
                    warnings,
                }
            }
            None => MessageView {
                body_text: m.snippet.clone(),
                msg: m,
                quoted_text: String::new(),
                body_html: None,
                attachments: Vec::new(),
                attachments_blocked: true,
                verdict: Verdict::NotScanned { reason: "body not downloaded".into() },
                warnings: Vec::new(),
                quote_missing: 0,
                remote_images: Vec::new(),
                invites: Vec::new(),
            },
        };
        views.push(view);
    }
    Ok(views)
}

// ---- server actions, queued while offline ---------------------------------------

/// A server-side action. Targets are (folder, UID) as they were when the
/// action was made, so later actions (e.g. a move) can't confuse replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PendingOp {
    Flag { targets: Vec<(String, u32)>, flag: Flag, on: bool },
    Move { targets: Vec<(String, u32)>, dest: String },
    Expunge { targets: Vec<(String, u32)> },
}

impl PendingOp {
    fn targets(&self) -> &[(String, u32)] {
        match self {
            PendingOp::Flag { targets, .. } | PendingOp::Move { targets, .. } | PendingOp::Expunge { targets } => targets,
        }
    }
}

fn targets(msgs: &[StoredMessage]) -> Vec<(String, u32)> {
    msgs.iter().filter_map(|m| m.uid.map(|u| (m.folder.clone(), u))).collect()
}

async fn apply_on_server(c: &mut ImapClient<MailStream>, store: &Mutex<Store>, account_id: i64, op: &PendingOp) -> Result<()> {
    let mut by_folder: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (folder, uid) in op.targets() {
        by_folder.entry(folder.as_str()).or_default().push(uid.to_string());
    }
    let dest_raw = match op {
        PendingOp::Move { dest, .. } => Some(ensure_imap_folder(c, store, account_id, dest).await?),
        _ => None,
    };
    for (folder, uids) in by_folder {
        let set = uids.join(",");
        c.select(&folder_raw(store, account_id, folder)?).await?;
        match op {
            PendingOp::Flag { flag, on, .. } => c.uid_store(&set, if *on { "+FLAGS.SILENT" } else { "-FLAGS.SILENT" }, flag.imap()).await?,
            PendingOp::Move { .. } => {
                let dest = dest_raw.as_deref().unwrap_or_default();
                if let Err(e) = c.uid_move(&set, dest).await {
                    // A folder made offline isn't on the server yet ([TRYCREATE]).
                    if !matches!(e, Error::Server(_)) {
                        return Err(e);
                    }
                    let _ = c.create(dest).await;
                    c.uid_move(&set, dest).await?;
                }
            }
            PendingOp::Expunge { .. } => {
                c.uid_store(&set, "+FLAGS.SILENT", "\\Deleted").await?;
                if c.has_cap("UIDPLUS") {
                    c.command_ok(&format!("UID EXPUNGE {set}")).await?;
                }
            }
        }
    }
    Ok(())
}

fn queue(store: &Mutex<Store>, account_id: i64, op: &PendingOp) -> Result<()> {
    lock(store)?.queue_op(account_id, &serde_json::to_string(op)?)
}

/// Runs the action on the server now, or queues it when the server can't be
/// reached (or older actions are still queued, to keep their order).
async fn run_or_queue(store: &Mutex<Store>, account: &Account, password: &Credential, op: PendingOp) -> Result<()> {
    if account.incoming_protocol != Protocol::Imap || op.targets().is_empty() {
        return Ok(());
    }
    if !lock(store)?.pending_ops(account.id)?.is_empty() {
        return queue(store, account.id, &op);
    }
    let mut c = match imap_session(account, password).await {
        Ok(c) => c,
        Err(e) if is_offline(&e) => return queue(store, account.id, &op),
        Err(e) => return Err(e),
    };
    match apply_on_server(&mut c, store, account.id, &op).await {
        Ok(()) => {
            let _ = c.logout().await;
            Ok(())
        }
        Err(e) if is_offline(&e) => queue(store, account.id, &op),
        Err(e) => Err(e),
    }
}

/// Replays queued actions in order. Stops (keeping the rest) if the server is
/// still unreachable; an action the server rejects is dropped so it can't
/// block the queue forever. Returns how many were applied.
pub async fn flush_pending(store: &Mutex<Store>, account: &Account, password: &Credential) -> Result<usize> {
    let queued = lock(store)?.pending_ops(account.id)?;
    if queued.is_empty() || account.incoming_protocol != Protocol::Imap {
        return Ok(0);
    }
    let mut c = match imap_session(account, password).await {
        Ok(c) => c,
        Err(e) if is_offline(&e) => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut applied = 0;
    for (id, data) in queued {
        let Ok(op) = serde_json::from_str::<PendingOp>(&data) else {
            lock(store)?.delete_op(id)?;
            continue;
        };
        match apply_on_server(&mut c, store, account.id, &op).await {
            Ok(()) => applied += 1,
            Err(e) if is_offline(&e) => break,
            Err(_) => {} // rejected (e.g. message already gone): drop it
        }
        lock(store)?.delete_op(id)?;
    }
    let _ = c.logout().await;
    Ok(applied)
}

pub async fn set_flag(store: &Mutex<Store>, account: &Account, password: &Credential, ids: &[i64], flag: Flag, on: bool) -> Result<()> {
    let msgs = lock(store)?.messages(ids)?;
    run_or_queue(store, account, password, PendingOp::Flag { targets: targets(&msgs), flag, on }).await?;
    let st = lock(store)?;
    for m in &msgs {
        match flag {
            Flag::Seen => st.set_seen(m.id, on)?,
            Flag::Flagged => st.set_flagged(m.id, on)?,
        }
    }
    Ok(())
}

pub async fn move_messages(store: &Mutex<Store>, account: &Account, password: &Credential, ids: &[i64], dest: &str) -> Result<()> {
    let msgs: Vec<StoredMessage> = lock(store)?.messages(ids)?.into_iter().filter(|m| m.folder != dest).collect();
    if msgs.is_empty() {
        return Ok(());
    }
    run_or_queue(store, account, password, PendingOp::Move { targets: targets(&msgs), dest: dest.to_string() }).await?;
    let st = lock(store)?;
    if st.folder(account.id, dest)?.is_none() {
        // Created on the server when the move runs.
        let raw = if account.incoming_protocol == Protocol::Imap { crate::imap_proto::utf7_encode(dest) } else { dest.to_string() };
        st.upsert_folder(account.id, dest, &raw, None, None)?;
    }
    for m in &msgs {
        st.set_folder(m.id, dest)?;
    }
    Ok(())
}

/// Moves to Trash; messages already in Trash are deleted permanently.
pub async fn delete_messages(store: &Mutex<Store>, account: &Account, password: &Credential, ids: &[i64]) -> Result<()> {
    let msgs = lock(store)?.messages(ids)?;
    let trash = lock(store)?.folder_by_special(account.id, "trash")?.map(|f| f.name).unwrap_or_else(|| "Trash".into());
    let (in_trash, rest): (Vec<_>, Vec<_>) = msgs.into_iter().partition(|m| m.folder == trash);
    if !rest.is_empty() {
        let ids: Vec<i64> = rest.iter().map(|m| m.id).collect();
        move_messages(store, account, password, &ids, &trash).await?;
    }
    if !in_trash.is_empty() {
        run_or_queue(store, account, password, PendingOp::Expunge { targets: targets(&in_trash) }).await?;
        let st = lock(store)?;
        for m in &in_trash {
            st.delete_message(m.id)?;
        }
    }
    Ok(())
}

/// "Run rules now" on an existing folder. Returns how many messages changed.
pub async fn run_rules_on_folder(store: &Mutex<Store>, account: &Account, password: &Credential, folder: &str) -> Result<usize> {
    let (msgs, rules, folders) = {
        let st = lock(store)?;
        (st.messages_in_folder(account.id, folder)?, st.rules_for(account.id)?, st.list_folders(account.id)?)
    };
    let mut moves: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let (mut read, mut flag) = (Vec::new(), Vec::new());
    for m in &msgs {
        let raw = lock(store)?.raw(m.id)?;
        let meta = raw.as_deref().map(parse_meta).unwrap_or_else(|| m.to_meta());
        let body = raw.as_deref().and_then(|r| parse_body(r).text);
        let out = evaluate(&rules, &meta, body.as_deref());
        if out.mark_read && !m.seen {
            read.push(m.id);
        }
        if out.flag && !m.flagged {
            flag.push(m.id);
        }
        if let Some(dest) = rule_destination(&out, &folders).filter(|d| d != folder) {
            moves.entry(dest).or_default().push(m.id);
        }
    }
    let changed = read.len() + flag.len() + moves.values().map(Vec::len).sum::<usize>();
    if !read.is_empty() {
        set_flag(store, account, password, &read, Flag::Seen, true).await?;
    }
    if !flag.is_empty() {
        set_flag(store, account, password, &flag, Flag::Flagged, true).await?;
    }
    for (dest, ids) in moves {
        move_messages(store, account, password, &ids, &dest).await?;
    }
    Ok(changed)
}

pub async fn send(store: &Mutex<Store>, account: &Account, password: &Credential, draft: &Draft) -> Result<SendResult> {
    let to = parse_address_list(&draft.to)?;
    let cc = parse_address_list(&draft.cc)?;
    let bcc = parse_address_list(&draft.bcc)?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(Error::Other("Add at least one recipient".into()));
    }
    let (parent, mut refs) = match draft.reply_to_message {
        Some(id) => {
            let st = lock(store)?;
            let p = st.message(id)?;
            let refs = st.references_of(id)?;
            (p, refs)
        }
        None => (None, Vec::new()),
    };
    if let Some(p) = &parent {
        refs.retain(|r| r != &p.message_id);
        refs.push(p.message_id.clone());
    }
    let from = Addr { name: account.name.clone(), email: account.email.clone() };
    let text = if account.signature.is_empty() { draft.body.clone() } else { format!("{}\n\n-- \n{}", draft.body, account.signature) };
    let (_, bytes) = build_message(&Outgoing {
        from: &from,
        to: &to,
        cc: &cc,
        subject: &draft.subject,
        text: &text,
        in_reply_to: parent.as_ref().map(|p| p.message_id.as_str()),
        references: &refs,
        calendar: None,
    })?;
    let rcpts: Vec<String> = to.iter().chain(&cc).chain(&bcc).map(|a| a.email.clone()).collect();
    deliver(store, account, password, &rcpts, bytes).await
}

/// Sends a built message, or keeps it in the Outbox when offline.
async fn deliver(store: &Mutex<Store>, account: &Account, password: &Credential, rcpts: &[String], bytes: Vec<u8>) -> Result<SendResult> {
    match crate::smtp::send_mail(&account.smtp, account.smtp_user(), password, &account.email, rcpts, &bytes).await {
        Ok(()) => {}
        Err(e) if is_offline(&e) => {
            queue_outgoing(store, account, rcpts, bytes)?;
            return Ok(SendResult::Queued);
        }
        Err(e) => return Err(e),
    }
    save_sent_copy(store, account, password, bytes).await;
    Ok(SendResult::Sent)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SendResult {
    Sent,
    /// Couldn't reach the server; kept in the Outbox and retried on sync.
    Queued,
}

/// Network-level failures (as opposed to the server rejecting the message).
fn is_offline(e: &Error) -> bool {
    matches!(e, Error::Io(_) | Error::Timeout)
}

const OUTBOX: &str = "Outbox";

fn queue_outgoing(store: &Mutex<Store>, account: &Account, rcpts: &[String], bytes: Vec<u8>) -> Result<()> {
    let st = lock(store)?;
    if st.folder(account.id, OUTBOX)?.is_none() {
        st.upsert_folder(account.id, OUTBOX, OUTBOX, None, Some("outbox"))?;
    }
    let meta = parse_meta(&bytes);
    let id = st.insert_message(&crate::store::NewMessage {
        account_id: account.id,
        folder: OUTBOX.into(),
        uid: None,
        pop_uidl: None,
        meta,
        seen: true,
        flagged: false,
        size: bytes.len() as u64,
        raw: Some(bytes),
    })?;
    st.queue_outgoing(account.id, id, rcpts)
}

/// Retries everything in the Outbox. Returns how many were sent.
pub async fn flush_outbox(store: &Mutex<Store>, account: &Account, password: &Credential) -> Result<usize> {
    let queued = lock(store)?.outbox(account.id)?;
    let mut sent = 0;
    for (qid, msg, rcpts) in queued {
        let Some(bytes) = lock(store)?.raw(msg)? else {
            lock(store)?.outbox_done(qid)?;
            continue;
        };
        match crate::smtp::send_mail(&account.smtp, account.smtp_user(), password, &account.email, &rcpts, &bytes).await {
            Ok(()) => {
                {
                    let st = lock(store)?;
                    st.outbox_done(qid)?;
                    st.delete_message(msg)?;
                }
                save_sent_copy(store, account, password, bytes).await;
                sent += 1;
            }
            Err(e) if is_offline(&e) => break, // still offline; try again next sync
            Err(e) => lock(store)?.outbox_failed(qid, &e.to_string())?,
        }
    }
    Ok(sent)
}

/// Best effort: a failure here must not turn a sent message into an error.
async fn save_sent_copy(store: &Mutex<Store>, account: &Account, password: &Credential, bytes: Vec<u8>) {
    let _ = save_sent_copy_inner(store, account, password, bytes).await;
}

async fn save_sent_copy_inner(store: &Mutex<Store>, account: &Account, password: &Credential, bytes: Vec<u8>) -> Result<()> {

    // Gmail stores sent mail itself; elsewhere keep a copy in Sent.
    let server_saves = account.smtp.host.ends_with("gmail.com") || account.smtp.host.ends_with("googlemail.com");
    if account.incoming_protocol == Protocol::Imap && !server_saves {
        let sent = lock(store)?.folder_by_special(account.id, "sent")?;
        if let Some(sent) = sent {
            let mut c = imap_session(account, password).await?;
            c.append(&sent.raw_name, "\\Seen", &bytes).await?;
            c.logout().await?;
        }
    } else if account.incoming_protocol == Protocol::Pop3 {
        let meta = parse_meta(&bytes);
        lock(store)?.insert_message(&crate::store::NewMessage {
            account_id: account.id,
            folder: "Sent".into(),
            uid: None,
            pop_uidl: None,
            meta,
            seen: true,
            flagged: false,
            size: bytes.len() as u64,
            raw: Some(bytes),
        })?;
    }
    Ok(())
}

// ---- saving attachments ----------------------------------------------------------

/// Saves attachment `index` of a message into `dir`, but only when the whole
/// message scans clean, the file type isn't flagged as dangerous, and the
/// attachment itself scans clean again. Never overwrites an existing file.
/// An attachment's name and contents, only once the message and the
/// attachment itself have passed every check (ClamAV, dangerous types).
async fn checked_attachment(store: &Mutex<Store>, message_id: i64, index: usize, scanner: &ScannerConfig) -> Result<(String, Vec<u8>)> {
    let raw = lock(store)?.raw(message_id)?.ok_or_else(|| Error::Other("The message isn't downloaded yet".into()))?;
    match scan::scan_cached(store, scanner, &raw).await {
        Verdict::Clean => {}
        Verdict::Infected { signature } => return Err(Error::Other(format!("Blocked: malware detected ({signature})"))),
        Verdict::NotScanned { reason } => return Err(Error::Other(format!("Can't save until the message has been scanned ({reason})"))),
    }
    let (name, bytes) = crate::message::attachment_bytes(&raw, index).ok_or_else(|| Error::Other("Attachment not found".into()))?;
    if let Some(why) = crate::safety::attachment_risk(&name) {
        return Err(Error::Other(format!("Blocked: {why}")));
    }
    if let Verdict::Infected { signature } = scan::scan_cached(store, scanner, &bytes).await {
        return Err(Error::Other(format!("Blocked: malware detected ({signature})")));
    }
    Ok((name, bytes))
}

pub async fn save_attachment(store: &Mutex<Store>, message_id: i64, index: usize, dir: &std::path::Path, scanner: &ScannerConfig) -> Result<std::path::PathBuf> {
    let (name, bytes) = checked_attachment(store, message_id, index, scanner).await?;
    let safe = safe_file_name(&name);
    let (stem, ext) = match safe.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (safe.clone(), String::new()),
    };
    std::fs::create_dir_all(dir)?;
    for n in 0..1000 {
        let file = if n == 0 { format!("{stem}{ext}") } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(file);
        // create_new: never replace (or follow a planted symlink to) a file.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(&bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Other("Too many files with that name".into()))
}

/// What a calendar attachment holds, for asking whether to add it.
#[derive(Debug, Clone, Serialize)]
pub struct IcsPreview {
    /// REQUEST, PUBLISH, CANCEL… (empty when the file doesn't say).
    pub method: String,
    /// One entry per event or series (changed instances left out).
    pub events: Vec<crate::ical::Event>,
    /// Every series in it is already in the Invitations calendar.
    pub in_calendar: bool,
}

async fn attachment_ics(store: &Mutex<Store>, message_id: i64, index: usize, scanner: &ScannerConfig) -> Result<String> {
    let (_, bytes) = checked_attachment(store, message_id, index, scanner).await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if !text.contains("BEGIN:VCALENDAR") || crate::ical::parse_events(&text).is_empty() {
        return Err(Error::Other("This isn't a calendar file Carteiro can read".into()));
    }
    Ok(text)
}

pub async fn preview_ics(store: &Mutex<Store>, message_id: i64, index: usize, scanner: &ScannerConfig) -> Result<IcsPreview> {
    let text = attachment_ics(store, message_id, index, scanner).await?;
    let events: Vec<_> = crate::ical::parse_events(&text).into_iter().filter(|e| e.recurrence_id.is_none()).collect();
    let st = lock(store)?;
    let in_calendar = events.iter().all(|e| !e.uid.is_empty() && st.invite_present(&e.uid).unwrap_or(false));
    Ok(IcsPreview { method: crate::ical::calendar_method(&text), events, in_calendar })
}

/// Adds (or, for a cancellation, applies) a calendar attachment, at your
/// request: dates you had removed from the series come back.
pub async fn add_ics(store: &Mutex<Store>, message_id: i64, index: usize, scanner: &ScannerConfig) -> Result<usize> {
    let text = attachment_ics(store, message_id, index, scanner).await?;
    let mut st = lock(store)?;
    let m = st.message(message_id)?;
    let sender = m.as_ref().map(|m| m.from.email.clone()).unwrap_or_default();
    crate::invites::import_ics(&mut st, &sender, &text, crate::calendar::now(), m.map(|m| m.account_id))
}

/// Sends your answer for one event (its date, in a series) to the
/// organizer, as an invitation reply calendars understand. Only when you ask.
pub async fn send_rsvp(store: &Mutex<Store>, account: &Account, password: &Credential, event_id: i64) -> Result<SendResult> {
    let (ev, organizer, sequence) = {
        let st = lock(store)?;
        let ev = st.event(event_id)?.ok_or_else(|| Error::Other("Event not found".into()))?;
        let (organizer, sequence, _) = st.invite_reply_info(&ev.remote_id)?.ok_or_else(|| Error::Other("Only invitations received by mail can be answered".into()))?;
        (ev, organizer, sequence)
    };
    let (partstat, word) = match ev.rsvp.as_str() {
        "yes" => ("ACCEPTED", "Accepted"),
        "maybe" => ("TENTATIVE", "Tentative"),
        "no" => ("DECLINED", "Declined"),
        _ => return Err(Error::Other("Choose Going, Maybe or Not going first".into())),
    };
    if !organizer.contains('@') {
        return Err(Error::Other("This invitation doesn't say who organized it".into()));
    }
    let instance = ev.event.recurring.then_some(&ev.event);
    let ics = crate::ical::build_reply(&ev.remote_id, sequence, &ev.event.title, &organizer, &account.email, &account.name, partstat, instance, crate::calendar::now());
    let from = Addr { name: account.name.clone(), email: account.email.clone() };
    let to = [Addr { name: String::new(), email: organizer.clone() }];
    let subject = format!("{word}: {}", ev.event.title);
    let text = format!("{} has {} this invitation.", if account.name.is_empty() { &account.email } else { &account.name }, word.to_lowercase());
    let (_, bytes) = build_message(&Outgoing { from: &from, to: &to, cc: &[], subject: &subject, text: &text, in_reply_to: None, references: &[], calendar: Some(&ics) })?;
    deliver(store, account, password, &[organizer], bytes).await
}

/// The account an invitation came to (to answer from it).
pub fn invite_account(store: &Mutex<Store>, event_id: i64) -> Result<Option<i64>> {
    let st = lock(store)?;
    let Some(ev) = st.event(event_id)? else { return Ok(None) };
    Ok(st.invite_reply_info(&ev.remote_id)?.and_then(|i| i.2))
}

// ---- offline copy -------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfflineReport {
    /// Message headers newly listed (older than the first sync window).
    pub listed: usize,
    /// Bodies downloaded.
    pub downloaded: usize,
    /// Cached bodies freed because they're older than the chosen period.
    pub freed: usize,
}

/// Special-use folders that only mirror other folders, or are local-only.
fn skip_folder(special: Option<&str>) -> bool {
    matches!(special, Some("all" | "flagged" | "important" | "outbox"))
}

/// Makes every message since `since` (None = everything) available offline,
/// and frees cached bodies older than that.
/// Where an offline download is, for showing progress.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OfflineProgress {
    pub account: String,
    pub folder: String,
    /// "listing" (finding older messages) or "downloading" (fetching bodies).
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
}

pub async fn download_offline(store: &Mutex<Store>, account: &Account, password: &Credential, since: Option<i64>) -> Result<OfflineReport> {
    download_offline_with(store, account, password, since, &|_| {}).await
}

/// `download_offline`, reporting progress as it goes.
pub async fn download_offline_with(store: &Mutex<Store>, account: &Account, password: &Credential, since: Option<i64>, progress: &(dyn Fn(OfflineProgress) + Send + Sync)) -> Result<OfflineReport> {
    let mut report = OfflineReport::default();
    let tell = |folder: &str, phase: &'static str, done: usize, total: usize| progress(OfflineProgress { account: account.email.clone(), folder: folder.to_string(), phase, done, total });
    if account.incoming_protocol == Protocol::Imap {
        let folders = lock(store)?.list_folders(account.id)?;
        let mut c = imap_session(account, password).await?;
        for f in folders.iter().filter(|f| !skip_folder(f.special.as_deref())) {
            if c.select(&f.raw_name).await.is_err() {
                continue; // e.g. \Noselect parents
            }
            let on_server = c.uid_search_since(since).await?;
            let known = lock(store)?.uids_in_folder(account.id, &f.name)?;
            let new: Vec<u32> = on_server.into_iter().filter(|u| !known.contains(u)).collect();
            let mut listed = 0;
            for chunk in new.chunks(200) {
                tell(&f.name, "listing", listed, new.len());
                listed += chunk.len();
                let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                for h in c.fetch_headers(&set).await? {
                    if h.has_flag("\\Deleted") {
                        continue;
                    }
                    let meta = parse_meta(h.header.as_deref().unwrap_or(&[]));
                    lock(store)?.insert_message(&crate::store::NewMessage {
                        account_id: account.id,
                        folder: f.name.clone(),
                        uid: Some(h.uid),
                        pop_uidl: None,
                        meta,
                        seen: h.has_flag("\\Seen"),
                        flagged: h.has_flag("\\Flagged"),
                        size: h.size,
                        raw: None,
                    })?;
                    report.listed += 1;
                }
            }
            let missing = lock(store)?.messages_missing_body(account.id, &f.name, since)?;
            for (i, chunk) in missing.chunks(25).enumerate() {
                tell(&f.name, "downloading", i * 25, missing.len());
                let set = chunk.iter().filter_map(|m| m.uid).map(|u| u.to_string()).collect::<Vec<_>>().join(",");
                if set.is_empty() {
                    continue;
                }
                for fetched in c.fetch_full(&set).await? {
                    if let (Some(body), Some(m)) = (fetched.body, chunk.iter().find(|m| m.uid == Some(fetched.uid))) {
                        lock(store)?.set_raw(m.id, &body)?;
                        report.downloaded += 1;
                    }
                }
            }
        }
        c.logout().await?;
    }
    if let Some(cutoff) = since {
        report.freed = lock(store)?.clear_bodies_before(account.id, cutoff)?;
    }
    Ok(report)
}

/// Lists up to `count` messages older than the oldest one known in `folder`
/// (the first sync lists only the latest ones). Headers only; bodies come
/// when opened. Returns how many were added: 0 means there are no more.
pub async fn load_older(store: &Mutex<Store>, account: &Account, password: &Credential, folder: &str, count: usize) -> Result<usize> {
    if account.incoming_protocol != Protocol::Imap {
        return Ok(0);
    }
    let raw_name = lock(store)?.folder(account.id, folder)?.map(|f| f.raw_name).unwrap_or_else(|| folder.to_string());
    let known = lock(store)?.uids_in_folder(account.id, folder)?;
    let oldest = known.iter().min().copied().unwrap_or(u32::MAX);
    let mut c = imap_session(account, password).await?;
    c.select(&raw_name).await?;
    let mut older: Vec<u32> = c.uid_search_since(None).await?.into_iter().filter(|u| *u < oldest && !known.contains(u)).collect();
    older.sort_unstable();
    let take: Vec<u32> = older.into_iter().rev().take(count).collect();
    let mut added = 0;
    for chunk in take.chunks(200) {
        let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        for h in c.fetch_headers(&set).await? {
            if h.has_flag("\\Deleted") {
                continue;
            }
            let meta = parse_meta(h.header.as_deref().unwrap_or(&[]));
            lock(store)?.insert_message(&crate::store::NewMessage {
                account_id: account.id, folder: folder.to_string(), uid: Some(h.uid), pop_uidl: None, meta,
                seen: h.has_flag("\\Seen"), flagged: h.has_flag("\\Flagged"), size: h.size, raw: None,
            })?;
            added += 1;
        }
    }
    c.logout().await?;
    Ok(added)
}

// ---- export to .mbox ----------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExportReport {
    pub path: String,
    pub folders: usize,
    pub messages: usize,
    /// Listed but whose body couldn't be downloaded.
    pub skipped: usize,
}

/// A file/folder name that is safe on every OS.
pub fn safe_file_name(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let mut name = cleaned.trim().trim_matches('.').to_string();
    // Room for " (999)" within the 255-byte limit of common file systems.
    while name.len() > 200 {
        let keep_ext = name.rsplit_once('.').map(|(_, e)| e.len() + 1).filter(|n| *n <= 16).unwrap_or(0);
        let mut cut = name.len() - keep_ext - 1;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name = format!("{}{}", &name[..cut], &name[name.len() - keep_ext..]);
    }
    // Windows device names (CON, NUL, COM1...) aren't files, with any extension.
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if device {
        name.insert(0, '_');
    }
    if name.is_empty() { "_".into() } else { name }
}

/// "From " line date (asctime, UTC): "Sun Nov  8 01:12:30 2026".
fn asctime(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = ts.div_euclid(86400);
    let (y, m, d) = crate::ical::civil_from_days(days);
    let s = ts.rem_euclid(86400);
    format!("{} {} {d:>2} {:02}:{:02}:{:02} {y}", DAYS[(days + 4).rem_euclid(7) as usize], MONTHS[m as usize - 1], s / 3600, s % 3600 / 60, s % 60)
}

/// Appends one message in mboxrd format (lines starting with ">*From " get
/// one more '>', so they can be unescaped exactly).
pub fn write_mbox_entry(out: &mut Vec<u8>, sender: &str, date: i64, raw: &[u8]) {
    let sender = if sender.is_empty() || sender.contains(char::is_whitespace) { "MAILER-DAEMON" } else { sender };
    out.extend_from_slice(format!("From {sender} {}\n", asctime(date)).as_bytes());
    let mut lines: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let unquoted = line.iter().position(|&b| b != b'>').map(|i| &line[i..]).unwrap_or(&[]);
        if unquoted.starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// Saves every message of the account (downloading any missing bodies first)
/// as one .mbox file per folder under `dir/<account>/`.
pub async fn export_mbox(store: &Mutex<Store>, account: &Account, password: &Credential, dir: &std::path::Path) -> Result<ExportReport> {
    // Best effort: offline export still writes whatever is cached.
    let _ = download_offline(store, account, password, None).await;
    let root = dir.join(safe_file_name(&account.email));
    std::fs::create_dir_all(&root)?;
    let mut report = ExportReport { path: root.display().to_string(), ..Default::default() };
    let folders = lock(store)?.list_folders(account.id)?;
    for f in folders.iter().filter(|f| !skip_folder(f.special.as_deref())) {
        let mut msgs = lock(store)?.messages_in_folder(account.id, &f.name)?;
        if msgs.is_empty() {
            continue;
        }
        msgs.sort_by_key(|m| m.date);
        let mut out = Vec::new();
        for m in &msgs {
            match lock(store)?.raw(m.id)? {
                Some(raw) => {
                    write_mbox_entry(&mut out, &m.from.email, m.date, &raw);
                    report.messages += 1;
                }
                None => report.skipped += 1,
            }
        }
        let name = safe_file_name(&f.name.replace(f.delimiter.as_deref().unwrap_or("/"), " - "));
        std::fs::write(root.join(format!("{name}.mbox")), out)?;
        report.folders += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Endpoint, Security};

    async fn fake_clamd(reply: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1 << 16];
                let mut got = Vec::new();
                while !got.ends_with(&[0, 0, 0, 0]) {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s.write_all(reply).await;
            }
        });
        addr
    }

    fn message_with(name: &str) -> Vec<u8> {
        format!(
            "Message-ID: <a@b>\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: application/octet-stream; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n--b--\r\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn attachments_save_only_when_safe() {
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let dir = std::env::temp_dir().join(format!("carteiro-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let insert = |raw: Vec<u8>| {
            let st = store.lock().unwrap();
            let mut a = crate::account::Account {
                id: 0, name: "n".into(), email: "e@x".into(), username: "e".into(), incoming_protocol: Protocol::Imap,
                incoming: crate::net::Endpoint { host: "h".into(), port: 1, security: crate::net::Security::Tls, allow_insecure: false },
                smtp: crate::net::Endpoint { host: "h".into(), port: 1, security: crate::net::Security::Tls, allow_insecure: false },
                smtp_username: None, signature: String::new(), auth: Default::default(),
            };
            st.add_account(&mut a).unwrap();
            st.insert_message(&crate::store::NewMessage {
                account_id: a.id, folder: "INBOX".into(), uid: Some(1), pop_uidl: None, meta: parse_meta(&raw),
                seen: true, flagged: false, size: raw.len() as u64, raw: Some(raw),
            }).unwrap()
        };
        let clean = ScannerConfig { clamd_addr: fake_clamd(b"stream: OK\0").await, ..Default::default() };
        let infected = ScannerConfig { clamd_addr: fake_clamd(b"stream: Eicar-Signature FOUND\0").await, ..Default::default() };

        let good = insert(message_with("../../report.txt"));
        let p1 = save_attachment(&store, good, 0, &dir, &clean).await.unwrap();
        assert_eq!(std::fs::read(&p1).unwrap(), b"Hello");
        assert!(p1.starts_with(&dir), "path traversal neutralised: {p1:?}");
        let p2 = save_attachment(&store, good, 0, &dir, &clean).await.unwrap();
        assert_ne!(p1, p2, "never overwrites");

        assert!(save_attachment(&store, good, 0, &dir, &infected).await.unwrap_err().to_string().contains("malware"));
        assert!(save_attachment(&store, good, 0, &dir, &ScannerConfig::default()).await.unwrap_err().to_string().contains("scanned"));
        let exe = insert(message_with("invoice.pdf.exe"));
        assert!(save_attachment(&store, exe, 0, &dir, &clean).await.unwrap_err().to_string().contains("disguised"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mboxrd_escaping() {
        let mut out = Vec::new();
        write_mbox_entry(&mut out, "a@b.c", 1794216600, b"Subject: x\r\n\r\nFrom the start\r\n>From quoted\r\nok\r\n");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "From a@b.c Mon Nov  9 09:30:00 2026\nSubject: x\n\n>From the start\n>>From quoted\nok\n\n");
        assert_eq!(safe_file_name("../INBOX/Sub:x"), "_INBOX_Sub_x");
    }

    #[tokio::test]
    async fn unreachable_smtp_queues_in_outbox() {
        let store = Mutex::new(Store::open_in_memory().unwrap());
        // Port 1 on localhost refuses connections: a network failure, not a rejection.
        let ep = Endpoint { host: "127.0.0.1".into(), port: 1, security: Security::Tls, allow_insecure: false };
        let mut account = Account {
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
        store.lock().unwrap().add_account(&mut account).unwrap();
        let draft = Draft {
            to: "you@y.org".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hi".into(),
            body: "offline".into(),
            reply_to_message: None,
        };
        assert_eq!(send(&store, &account, &Credential::Password("pw".into()), &draft).await.unwrap(), SendResult::Queued);
        {
            let st = store.lock().unwrap();
            let q = st.outbox(account.id).unwrap();
            assert_eq!(q.len(), 1);
            assert_eq!(q[0].2, vec!["you@y.org".to_string()]);
            assert_eq!(st.messages_in_folder(account.id, "Outbox").unwrap().len(), 1);
        }
        // Still offline: flushing keeps it queued.
        assert_eq!(flush_outbox(&store, &account, &Credential::Password("pw".into())).await.unwrap(), 0);
        assert_eq!(store.lock().unwrap().outbox(account.id).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn quoted_levels_missing_from_the_conversation() {
        let refs: Vec<String> = ["root", "a", "b"].map(String::from).to_vec(); // b is the parent
        let have = |ids: &[&'static str]| ids.iter().copied().collect::<HashSet<&str>>();
        assert_eq!(missing_quote_levels(&refs, &have(&["root", "a", "b"]), true), 0, "parent here: fold it all");
        assert_eq!(missing_quote_levels(&refs, &have(&["root", "a"]), true), 1, "only the parent is missing");
        assert_eq!(missing_quote_levels(&refs, &have(&["root"]), true), 2);
        assert_eq!(missing_quote_levels(&refs, &have(&["root", "b"]), true), 0, "a gap further up doesn't matter: b quotes it");
        assert_eq!(missing_quote_levels(&[], &have(&[]), true), u32::MAX, "a quote without references: show it");
        assert_eq!(missing_quote_levels(&[], &have(&[]), false), 0);
    }
}

#[cfg(test)]
mod file_name_tests {
    use super::safe_file_name;

    #[test]
    fn attachment_names_cannot_escape_or_name_devices() {
        for (hostile, expected) in [
            ("../../../../home/user/.bashrc", "_.._.._.._home_user_.bashrc"),
            ("..\\..\\Windows\\System32\\evil.dll", "_.._Windows_System32_evil.dll"),
            ("/etc/passwd", "_etc_passwd"),
            ("C:\\Users\\Public\\x.txt", "C__Users_Public_x.txt"),
            ("...", "_"),
            ("report.pdf\u{0}.exe", "report.pdf_.exe"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("com1.pdf", "_com1.pdf"),
            ("LPT9", "_LPT9"),
            ("console.log", "console.log"),
        ] {
            let got = safe_file_name(hostile);
            assert_eq!(got, expected, "{hostile:?}");
            assert!(!got.contains('/') && !got.contains('\\') && !got.starts_with('.'), "{hostile:?} -> {got:?}");
        }
        let long = format!("{}.pdf", "é".repeat(300));
        let got = safe_file_name(&long);
        assert!(got.len() <= 200 && got.ends_with(".pdf"), "{} bytes", got.len());
    }

    #[test]
    fn clean_invitations_reach_the_calendar_and_say_so() {
        use super::*;
        let store = std::sync::Mutex::new(Store::open_in_memory().unwrap());
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        let raw = b"From: Ana <ana@x.org>\r\nSubject: Invitation\r\nMessage-ID: <i1@x.org>\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:c-1\r\nSUMMARY:Lunch\r\nDTSTART:20991109T120000Z\r\nDTEND:20991109T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let id = {
            let st = store.lock().unwrap();
            st.add_account(&mut a).unwrap();
            st.insert_message(&crate::store::NewMessage { account_id: a.id, folder: "INBOX".into(), uid: Some(1), pop_uidl: None, meta: parse_meta(raw), seen: false, flagged: false, size: raw.len() as u64, raw: None }).unwrap()
        };
        take_invites_changed();
        on_clean(&store, id, raw).unwrap();
        assert!(take_invites_changed(), "the open calendar is told");
        assert!(!take_invites_changed(), "once");
        let st = store.lock().unwrap();
        assert_eq!(st.invites_of(id, 0).unwrap()[0].event.title, "Lunch");
        drop(st);
        on_clean(&store, id, raw).unwrap();
        assert!(!take_invites_changed(), "nothing new the second time");
    }

    #[test]
    fn words_inside_attachments_are_searchable_and_read_once() {
        use super::*;
        let store = std::sync::Mutex::new(Store::open_in_memory().unwrap());
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        store.lock().unwrap().add_account(&mut a).unwrap();
        let docx = crate::doctext::tests::zip(&[("word/document.xml", "<w:p><w:t>Kangaroo migration budget</w:t></w:p>")], true);
        let mail = |mid: &str| {
            let b64 = crate::b64::encode(&docx);
            format!("From: a@x.org\r\nSubject: Report {mid}\r\nMessage-ID: <{mid}@x.org>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--b\r\nContent-Type: application/vnd.openxmlformats-officedocument.wordprocessingml.document; name=\"Plan Q3.docx\"\r\nContent-Disposition: attachment; filename=\"Plan Q3.docx\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{b64}\r\n--b--\r\n").into_bytes()
        };
        for (uid, mid) in [(1, "m1"), (2, "m2")] {
            let raw = mail(mid);
            let id = store.lock().unwrap().insert_message(&crate::store::NewMessage {
                account_id: a.id, folder: "INBOX".into(), uid: Some(uid), pop_uidl: None, meta: parse_meta(&raw), seen: false, flagged: false, size: raw.len() as u64, raw: None,
            }).unwrap();
            on_clean(&store, id, &raw).unwrap();
            if uid == 1 {
                // Prove the second message uses the stored text: change it.
                let hash = crate::update::sha256_hex(&docx);
                store.lock().unwrap().set_attachment_text(&hash, "Kangaroo migration budget (remembered)").unwrap();
            }
        }
        let st = store.lock().unwrap();
        let found = |q: &str| st.list_threads(a.id, "INBOX", Some(q), 10, 0).unwrap().len();
        assert_eq!(found("kangaroo"), 2, "a word inside the Word file");
        assert_eq!(found("remembered"), 1, "the same file in another message isn't read again");
        assert_eq!(found("\"Plan Q3\""), 2, "attachment names too");
    }

    #[test]
    fn search_marks_the_original_not_replies_that_quote_it() {
        use super::*;
        let st = Store::open_in_memory().unwrap();
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        st.add_account(&mut a).unwrap();
        let pdf = crate::b64::encode(b"%PDF-1.4 the handover plan");
        let mail = |mid: &str, refs: &str, date: &str, text: &str, attach: bool| {
            let mut m = format!("From: {}\r\nSubject: Re: Project\r\nMessage-ID: <{mid}@x>\r\n{refs}Date: {date}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n{text}\r\n", if mid == "o" { "ana@x.org" } else { "bob@x.org" });
            if attach {
                m += &format!("--b\r\nContent-Type: application/pdf; name=\"handover.pdf\"\r\nContent-Disposition: attachment; filename=\"handover.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{pdf}\r\n");
            }
            m += "--b--\r\n";
            m.into_bytes()
        };
        let msgs = [
            mail("o", "", "Mon, 2 Nov 2026 10:00:00 +0000", "Here is the handover plan.", true),
            mail("r", "In-Reply-To: <o@x>\r\n", "Tue, 3 Nov 2026 10:00:00 +0000", "Thanks, looks good.\n\nOn Mon, Ana wrote:\n> Here is the handover plan.", true),
            mail("n", "In-Reply-To: <r@x>\r\n", "Wed, 4 Nov 2026 10:00:00 +0000", "Separate handover note from me.", false),
        ];
        let mut ids = Vec::new();
        for (uid, raw) in msgs.iter().enumerate() {
            ids.push(st.insert_message(&crate::store::NewMessage { account_id: a.id, folder: "INBOX".into(), uid: Some(uid as u32 + 1), pop_uidl: None, meta: parse_meta(raw), seen: true, flagged: false, size: 1, raw: Some(raw.clone()) }).unwrap());
        }
        let key = st.message(ids[0]).unwrap().unwrap().thread_key;
        let hits = |q: &str| own_hits(&st, a.id, &key, &[ids[2], ids[1], ids[0]], q).unwrap();
        assert_eq!(hits("has:pdf"), [ids[0]], "the reply only re-sent the same file");
        assert_eq!(hits("has:attachment:pdf handover"), [ids[0]]);
        assert_eq!(hits("handover"), [ids[2], ids[0]], "the reply only quotes it; the newest says it itself");
        assert_eq!(hits("looks good"), [ids[1]]);
        assert_eq!(hits("nowhere-to-be-found"), [ids[2], ids[1], ids[0]], "nothing left: the search's own matches stand");
    }
}
