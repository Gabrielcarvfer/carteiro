//! Pulls new mail from the server into the store, applying inbox rules as
//! messages arrive (server-side moves for IMAP, local folders for POP3).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::account::{Account, Credential, Protocol};
use crate::error::{Error, Result};
use crate::imap::{Fetched, ImapClient};
use crate::imap_proto::utf7_encode;
use crate::message::{parse_body, parse_meta};
use crate::net::MailStream;
use crate::pop3::Pop3Client;
use crate::rules::{evaluate, Outcome, Rule};
use crate::store::{FolderInfo, NewMessage, Store};

/// How many of the newest messages to pull per folder on first sync.
const INITIAL_WINDOW: u32 = 500;
/// Bodies up to this size are downloaded during sync (instant opening,
/// body-based rules); larger ones are fetched on demand.
const PREFETCH_MAX: u64 = 512 * 1024;
const POP_BATCH: usize = 300;

#[derive(Debug, Default, Clone, Serialize)]
pub struct SyncReport {
    pub new_messages: usize,
    pub moved_by_rules: usize,
    pub folders: usize,
    pub errors: Vec<String>,
    /// Unread mail that landed in the Inbox during an incremental sync (never
    /// the initial download), for desktop notifications. Capped at 20.
    pub arrived: Vec<Arrived>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Arrived {
    pub account_id: i64,
    pub from: String,
    pub subject: String,
}

const MAX_ARRIVED: usize = 20;

fn note_arrival(report: &mut SyncReport, account_id: i64, meta: &crate::message::MessageMeta) {
    if report.arrived.len() < MAX_ARRIVED {
        report.arrived.push(Arrived { account_id, from: meta.from.display().to_string(), subject: meta.subject.clone() });
    }
}

pub fn lock(store: &Mutex<Store>) -> Result<MutexGuard<'_, Store>> {
    store.lock().map_err(|_| Error::Other("database lock poisoned".into()))
}

pub async fn imap_session(account: &Account, password: &Credential) -> Result<ImapClient<MailStream>> {
    let mut c = ImapClient::connect(&account.incoming).await?;
    c.authenticate(&account.username, password).await?;
    Ok(c)
}

/// User names worth trying, in order: what was typed, then the other common
/// form (full address <-> part before the @). Servers disagree on which one
/// they want, so setup tries both instead of failing.
pub fn username_candidates(typed: &str, email: &str) -> Vec<String> {
    let typed = typed.trim();
    let mut out = vec![typed.to_string()];
    let alt = match typed.split_once('@') {
        Some((local, _)) => local.to_string(),
        None if email.contains('@') && email.split('@').next() == Some(typed) => email.trim().to_string(),
        None => String::new(),
    };
    if !alt.is_empty() && alt != typed {
        out.push(alt);
    }
    out
}

async fn login_incoming(account: &Account, user: &str, password: &Credential) -> Result<()> {
    match account.incoming_protocol {
        Protocol::Imap => {
            let mut c = ImapClient::connect(&account.incoming).await?;
            c.authenticate(user, password).await?;
            c.logout().await
        }
        Protocol::Pop3 => {
            let mut c = Pop3Client::connect(&account.incoming).await?;
            c.authenticate(user, password).await?;
            c.quit().await
        }
    }
}

/// Tries each candidate until one is accepted. Only authentication failures
/// move on to the next candidate; network/TLS errors are reported at once.
async fn first_accepted<F, Fut>(candidates: &[String], side: &str, host: &str, mut attempt: F) -> Result<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    for user in candidates {
        match attempt(user.clone()).await {
            Ok(()) => return Ok(user.clone()),
            Err(Error::Auth(_)) => continue,
            Err(e) => return Err(Error::Other(format!("Couldn't connect to the {side} server {host}: {e}"))),
        }
    }
    let tried = candidates.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(" and ");
    Err(Error::Auth(format!(
        "the {side} server {host} rejected the password (tried user name {tried}). Check the password, or whether your provider needs an app password."
    )))
}

/// Checks that both incoming and outgoing servers accept the credentials and
/// returns the account with the user names that actually worked.
pub async fn verify_account(account: &Account, password: &Credential) -> Result<Account> {
    let mut account = account.clone();
    let incoming = username_candidates(&account.username, &account.email);
    let side = match account.incoming_protocol {
        Protocol::Imap => "incoming (IMAP)",
        Protocol::Pop3 => "incoming (POP3)",
    };
    let acct = account.clone();
    account.username = first_accepted(&incoming, side, &account.incoming.host, |u| {
        let acct = acct.clone();
        async move { login_incoming(&acct, &u, password).await }
    })
    .await?;

    // SMTP usually takes the same user name as IMAP, but not always.
    let mut smtp = vec![account.username.clone()];
    for c in username_candidates(account.smtp_user(), &account.email) {
        if !smtp.contains(&c) {
            smtp.push(c);
        }
    }
    let ep = account.smtp.clone();
    let smtp_user = first_accepted(&smtp, "outgoing (SMTP)", &ep.host, |u| {
        let ep = ep.clone();
        async move { crate::smtp::verify(&ep, &u, password).await }
    })
    .await?;
    account.smtp_username = (smtp_user != account.username).then_some(smtp_user);
    Ok(account)
}


pub async fn sync_account(store: &Mutex<Store>, account: &Account, password: &Credential) -> Result<SyncReport> {
    // Replay actions and send mail queued while offline, first.
    let replayed = crate::ops::flush_pending(store, account, password).await;
    let sent = crate::ops::flush_outbox(store, account, password).await;
    let mut report = match account.incoming_protocol {
        Protocol::Imap => sync_imap(store, account, password).await?,
        Protocol::Pop3 => sync_pop3(store, account, password).await?,
    };
    if let Err(e) = sent {
        report.errors.push(format!("Outbox: {e}"));
    }
    if let Err(e) = replayed {
        report.errors.push(format!("Offline changes: {e}"));
    }
    Ok(report)
}

/// Resolves a rule outcome to a destination folder, if any.
pub fn rule_destination(out: &Outcome, folders: &[FolderInfo]) -> Option<String> {
    if out.delete {
        return Some(folders.iter().find(|f| f.special.as_deref() == Some("trash")).map(|f| f.name.clone()).unwrap_or_else(|| "Trash".into()));
    }
    let want = out.move_to.as_ref()?;
    Some(folders.iter().find(|f| f.name.eq_ignore_ascii_case(want)).map(|f| f.name.clone()).unwrap_or_else(|| want.clone()))
}

/// Raw (wire) name of `name`, creating the folder on the server if needed.
pub async fn ensure_imap_folder(c: &mut ImapClient<MailStream>, store: &Mutex<Store>, account_id: i64, name: &str) -> Result<String> {
    if let Some(f) = lock(store)?.folder(account_id, name)? {
        return Ok(f.raw_name);
    }
    let raw = utf7_encode(name);
    c.create(&raw).await?;
    lock(store)?.upsert_folder(account_id, name, &raw, None, None)?;
    Ok(raw)
}

async fn sync_imap(store: &Mutex<Store>, account: &Account, password: &Credential) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let mut c = imap_session(account, password).await?;
    let server_folders = c.list().await?;
    {
        let st = lock(store)?;
        for f in &server_folders {
            st.upsert_folder(account.id, &f.name, &f.raw_name, f.delimiter.as_deref(), f.special_use())?;
        }
        st.retain_folders(account.id, &server_folders.iter().map(|f| f.name.clone()).collect::<Vec<_>>())?;
    }
    let rules = lock(store)?.rules_for(account.id)?;
    let mut order: Vec<_> = server_folders.iter().filter(|f| f.selectable() && !f.is_virtual()).collect();
    order.sort_by_key(|f| if f.special_use() == Some("inbox") { 0 } else { 1 });
    for f in order {
        if let Err(e) = sync_imap_folder(&mut c, store, account, &f.name, &f.raw_name, &rules, &mut report).await {
            report.errors.push(format!("{}: {e}", f.name));
        }
        report.folders += 1;
    }
    c.logout().await?;
    Ok(report)
}

async fn sync_imap_folder(
    c: &mut ImapClient<MailStream>,
    store: &Mutex<Store>,
    account: &Account,
    name: &str,
    raw_name: &str,
    rules: &[Rule],
    report: &mut SyncReport,
) -> Result<()> {
    let info = c.select(raw_name).await?;
    let (validity, mut last_uid) = lock(store)?.folder_state(account.id, name)?;
    if validity != info.uidvalidity {
        lock(store)?.reset_folder(account.id, name, info.uidvalidity)?;
        last_uid = 0;
    }
    if info.exists == 0 {
        return Ok(());
    }
    let start = if last_uid == 0 { info.uidnext.saturating_sub(INITIAL_WINDOW).max(1) } else { last_uid + 1 };
    if info.uidnext != 0 && start >= info.uidnext {
        return Ok(());
    }
    let mut new: Vec<Fetched> = c.fetch_headers(&format!("{start}:*")).await?.into_iter().filter(|h| h.uid >= start).collect();
    if new.is_empty() {
        return Ok(());
    }
    new.sort_by_key(|h| h.uid);
    let max_uid = new.last().map_or(last_uid, |h| h.uid);

    let small: Vec<String> = new.iter().filter(|h| h.size <= PREFETCH_MAX).map(|h| h.uid.to_string()).collect();
    let mut bodies: HashMap<u32, Vec<u8>> = HashMap::new();
    for chunk in small.chunks(50) {
        for f in c.fetch_full(&chunk.join(",")).await? {
            if let Some(b) = f.body {
                bodies.insert(f.uid, b);
            }
        }
    }

    let apply_rules = name.eq_ignore_ascii_case("INBOX") && !rules.is_empty();
    for h in new {
        if h.has_flag("\\Deleted") {
            continue;
        }
        let raw = bodies.remove(&h.uid);
        let meta = parse_meta(raw.as_deref().or(h.header.as_deref()).unwrap_or(&[]));
        let mut seen = h.has_flag("\\Seen");
        let mut flagged = h.has_flag("\\Flagged");
        if apply_rules {
            let body = raw.as_deref().and_then(|r| parse_body(r).text);
            let out = evaluate(rules, &meta, body.as_deref());
            if !out.is_empty() {
                let uid = h.uid.to_string();
                if out.mark_read && !seen {
                    c.uid_store(&uid, "+FLAGS.SILENT", "\\Seen").await?;
                    seen = true;
                }
                if out.flag && !flagged {
                    c.uid_store(&uid, "+FLAGS.SILENT", "\\Flagged").await?;
                    flagged = true;
                }
                for copy in &out.copy_to {
                    let dest = ensure_imap_folder(c, store, account.id, copy).await?;
                    c.uid_copy(&uid, &dest).await?;
                }
                let folders = lock(store)?.list_folders(account.id)?;
                if let Some(dest) = rule_destination(&out, &folders).filter(|d| d != name) {
                    let dest_raw = ensure_imap_folder(c, store, account.id, &dest).await?;
                    c.uid_move(&uid, &dest_raw).await?;
                    report.moved_by_rules += 1;
                    // It will be picked up when the destination folder syncs.
                    continue;
                }
            }
        }
        if last_uid != 0 && !seen && name.eq_ignore_ascii_case("INBOX") {
            note_arrival(report, account.id, &meta);
        }
        lock(store)?.insert_message(&NewMessage {
            account_id: account.id,
            folder: name.to_string(),
            uid: Some(h.uid),
            pop_uidl: None,
            meta,
            seen,
            flagged,
            size: h.size,
            raw,
        })?;
        report.new_messages += 1;
    }
    lock(store)?.set_last_uid(account.id, name, max_uid)?;
    Ok(())
}

async fn sync_pop3(store: &Mutex<Store>, account: &Account, password: &Credential) -> Result<SyncReport> {
    let mut report = SyncReport { folders: 1, ..Default::default() };
    let mut c = Pop3Client::connect(&account.incoming).await?;
    c.authenticate(&account.username, password).await?;
    let list = c.uidl().await?;
    let (known, rules) = {
        let st = lock(store)?;
        for (name, special) in [("INBOX", "inbox"), ("Sent", "sent"), ("Trash", "trash")] {
            if st.folder(account.id, name)?.is_none() {
                st.upsert_folder(account.id, name, name, None, Some(special))?;
            }
        }
        (st.known_uidls(account.id)?, st.rules_for(account.id)?)
    };
    // Newest first, in batches, so a huge mailbox backfills over several syncs.
    let todo: Vec<_> = list.into_iter().rev().filter(|(_, u)| !known.contains(u)).take(POP_BATCH).collect();
    for (n, uidl) in todo {
        let raw = c.retr(n).await?;
        let meta = parse_meta(&raw);
        let body = parse_body(&raw).text;
        let out = evaluate(&rules, &meta, body.as_deref());
        let st = lock(store)?;
        let folders = st.list_folders(account.id)?;
        let folder = match rule_destination(&out, &folders) {
            Some(dest) => {
                if st.folder(account.id, &dest)?.is_none() {
                    st.upsert_folder(account.id, &dest, &dest, None, None)?;
                }
                report.moved_by_rules += 1;
                dest
            }
            None => "INBOX".to_string(),
        };
        if !known.is_empty() && !out.mark_read && folder == "INBOX" {
            note_arrival(&mut report, account.id, &meta);
        }
        st.insert_message(&NewMessage {
            account_id: account.id,
            folder,
            uid: None,
            pop_uidl: Some(uidl),
            meta,
            seen: out.mark_read,
            flagged: out.flag,
            size: raw.len() as u64,
            raw: Some(raw),
        })?;
        report.new_messages += 1;
    }
    c.quit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_cover_both_forms() {
        assert_eq!(username_candidates("me@x.org", "me@x.org"), vec!["me@x.org", "me"]);
        assert_eq!(username_candidates("me", "me@x.org"), vec!["me", "me@x.org"]);
        assert_eq!(username_candidates("other", "me@x.org"), vec!["other"]);
        assert_eq!(username_candidates(" me@x.org ", "me@x.org"), vec!["me@x.org", "me"]);
    }
}
