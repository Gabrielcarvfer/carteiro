//! Export / import of mail and calendar settings. Exports never contain
//! passwords or sign-in tokens: after importing, passwords are entered again
//! (and Google / Microsoft accounts sign in again).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::{Account, AuthMethod};
use crate::error::{Error, Result};
use crate::rules::Rule;
use crate::store::Store;

const FORMAT: &str = "carteiro-settings";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedRule {
    /// The account the rule belongs to (by address), or None for all.
    pub account: Option<String>,
    pub rule: Rule,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedCalendar {
    pub kind: String,
    /// For calendars that come with a mail account.
    pub account: Option<String>,
    pub url: String,
    pub username: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exported {
    pub format: String,
    pub version: u32,
    pub exported_at: i64,
    pub accounts: Vec<Account>,
    pub rules: Vec<ExportedRule>,
    pub calendars: Vec<ExportedCalendar>,
    /// App and UI preferences, opaque to the engine.
    #[serde(default)]
    pub app: Value,
}

pub fn export(store: &Store, app: Value, now: i64) -> Result<Exported> {
    let accounts = store.list_accounts()?;
    let email_of = |id: Option<i64>| id.and_then(|i| accounts.iter().find(|a| a.id == i)).map(|a| a.email.clone());
    let rules = store
        .list_rules()?
        .into_iter()
        .map(|mut r| {
            let account = email_of(r.account_id);
            r.id = None;
            r.account_id = None;
            ExportedRule { account, rule: r }
        })
        .collect();
    let calendars = store
        .cal_sources()?
        .into_iter()
        .filter(|c| c.kind != "local")
        .map(|c| ExportedCalendar { account: email_of(c.account_id), kind: c.kind, url: c.url, username: c.username, name: c.name })
        .collect();
    let accounts = accounts.into_iter().map(|mut a| {
        a.id = 0;
        a
    });
    Ok(Exported { format: FORMAT.into(), version: VERSION, exported_at: now, accounts: accounts.collect(), rules, calendars, app })
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    pub accounts_added: Vec<String>,
    /// Already present, left unchanged.
    pub accounts_existing: Vec<String>,
    /// Need a password (or a fresh sign-in) before they can sync.
    pub needs_password: Vec<String>,
    pub needs_sign_in: Vec<String>,
    pub rules_added: usize,
    pub calendars_added: usize,
    /// Calendars that need credentials and must be re-added by hand.
    pub calendars_to_readd: Vec<String>,
}

pub fn parse(json: &str) -> Result<Exported> {
    let data: Exported = serde_json::from_str(json).map_err(|e| Error::Other(format!("Not a Carteiro settings file: {e}")))?;
    if data.format != FORMAT {
        return Err(Error::Other("Not a Carteiro settings file".into()));
    }
    if data.version > VERSION {
        return Err(Error::Other("This settings file is from a newer Carteiro; update the app first".into()));
    }
    Ok(data)
}

/// Adds what isn't there yet. Existing accounts (same address) are kept.
pub fn import(store: &Store, data: &Exported) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let existing = store.list_accounts()?;
    for a in &data.accounts {
        if existing.iter().any(|e| e.email.eq_ignore_ascii_case(&a.email)) {
            report.accounts_existing.push(a.email.clone());
            continue;
        }
        let mut a = a.clone();
        a.id = 0;
        store.add_account(&mut a)?;
        match a.auth {
            AuthMethod::Password => report.needs_password.push(a.email.clone()),
            _ => report.needs_sign_in.push(a.email.clone()),
        }
        report.accounts_added.push(a.email);
    }
    let accounts = store.list_accounts()?;
    let id_of = |email: &Option<String>| email.as_ref().and_then(|e| accounts.iter().find(|a| a.email.eq_ignore_ascii_case(e))).map(|a| a.id);

    let current = store.list_rules()?;
    for r in &data.rules {
        let mut rule = r.rule.clone();
        rule.id = None;
        rule.account_id = id_of(&r.account);
        if r.account.is_some() && rule.account_id.is_none() {
            continue; // its account wasn't imported
        }
        // Skip exact duplicates (importing the same file twice).
        if current.iter().any(|c| c.name == rule.name && c.conditions == rule.conditions && c.actions == rule.actions && c.account_id == rule.account_id) {
            continue;
        }
        if store.save_rule(&mut rule).is_ok() {
            report.rules_added += 1;
        }
    }

    let sources = store.cal_sources()?;
    for c in &data.calendars {
        let account_id = id_of(&c.account);
        if sources.iter().any(|s| s.kind == c.kind && s.url == c.url && s.username == c.username && s.account_id == account_id) {
            continue;
        }
        match c.kind.as_str() {
            // No credentials needed: restore as-is.
            "holidays" | "ics" => {
                store.add_cal_source(&c.kind, None, &c.url, &c.username, &c.name)?;
                report.calendars_added += 1;
            }
            // Comes back with its account's sign-in.
            "google" | "microsoft" if account_id.is_some() => {
                store.add_cal_source(&c.kind, account_id, &c.url, &c.username, &c.name)?;
                report.calendars_added += 1;
            }
            _ => report.calendars_to_readd.push(c.name.clone()),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Protocol;
    use crate::net::{Endpoint, Security};

    fn acct(email: &str) -> Account {
        let ep = Endpoint { host: "h".into(), port: 993, security: Security::Tls, allow_insecure: false };
        Account {
            id: 0,
            name: "N".into(),
            email: email.into(),
            username: email.into(),
            incoming_protocol: Protocol::Imap,
            incoming: ep.clone(),
            smtp: ep,
            smtp_username: None,
            signature: "--\nme".into(),
            auth: AuthMethod::Password,
        }
    }

    #[test]
    fn roundtrip_without_secrets() {
        let src = Store::open_in_memory().unwrap();
        let mut a = acct("me@x.org");
        src.add_account(&mut a).unwrap();
        src.set_secret("account-1", "hunter2").unwrap();
        let mut r = Rule::move_from_sender("boss@x.org", "Boss");
        r.account_id = Some(a.id);
        src.save_rule(&mut r).unwrap();
        src.add_cal_source("holidays", None, "ES/ES-CT", "", "Holidays: Spain").unwrap();
        src.add_cal_source("caldav", None, "https://dav.x/", "me", "Work").unwrap();

        let json = serde_json::to_string(&export(&src, serde_json::json!({"week_start": 0}), 1).unwrap()).unwrap();
        assert!(!json.contains("hunter2"), "secrets never exported");

        let dst = Store::open_in_memory().unwrap();
        let data = parse(&json).unwrap();
        let rep = import(&dst, &data).unwrap();
        assert_eq!(rep.accounts_added, ["me@x.org"]);
        assert_eq!(rep.needs_password, ["me@x.org"]);
        assert_eq!(rep.rules_added, 1);
        assert_eq!(rep.calendars_added, 1);
        assert_eq!(rep.calendars_to_readd, ["Work"]);
        let imported = dst.list_accounts().unwrap();
        assert_eq!(imported[0].signature, "--\nme");
        assert_eq!(dst.list_rules().unwrap()[0].account_id, Some(imported[0].id));

        // Importing again changes nothing.
        let again = import(&dst, &data).unwrap();
        assert_eq!((again.accounts_added.len(), again.rules_added, again.calendars_added), (0, 0, 0));
        assert!(parse("{\"format\":\"other\"}").is_err());
    }
}
