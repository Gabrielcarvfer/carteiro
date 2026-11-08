//! Full backups: everything this install holds, sealed with a passphrase
//! (mail_core::backup): the database plus the passwords and sign-ins kept in
//! the system keyring. Restoring one on any install, on any platform, takes
//! over all of it, so losing a device or the app's signing key loses nothing.

use crate::{err, restrict_to_owner, secrets, Res};
use mail_core::store::Store;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Upload target for a backup file sent from the interface.
pub const UPLOAD: &str = "restore-upload.carteiro";
/// Kept in the backup's database but never moved to the keyring.
const NOT_KEYRING: &[&str] = &["app_settings", "scanner_config", "ui_prefs"];

fn lock(m: &Mutex<Store>) -> Res<std::sync::MutexGuard<'_, Store>> {
    m.lock().map_err(|_| "lock poisoned".into())
}

/// Secrets that live outside the database on desktop (see secrets.rs).
fn keyring_keys(st: &Store) -> Res<Vec<String>> {
    let mut keys = Vec::new();
    for a in st.list_accounts().map_err(err)? {
        keys.push(secrets::password_key(a.id));
        keys.push(format!("oauth-acct-{}", a.id));
    }
    for c in st.cal_sources().map_err(err)? {
        keys.push(format!("cal-{}", c.id));
        keys.push(format!("oauth-src-{}", c.id));
    }
    for c in st.contact_sources().map_err(err)? {
        keys.push(format!("contacts-{}", c.id));
    }
    Ok(keys)
}

fn remove_db(path: &Path) {
    for f in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{f}", path.display()));
    }
}

/// Writes `Carteiro backup <stamp>.carteiro` into `dest_dir`. Blocking.
pub fn create(data: &Path, store: &Mutex<Store>, dest_dir: &Path, passphrase: &str, ui: &serde_json::Value, stamp: &str) -> Res<PathBuf> {
    if passphrase.chars().count() < mail_core::backup::MIN_PASSPHRASE {
        return Err(format!("Use a passphrase of at least {} characters", mail_core::backup::MIN_PASSPHRASE));
    }
    if !dest_dir.is_dir() {
        return Err(format!("The folder {} doesn't exist", dest_dir.display()));
    }
    // A plain copy with the passwords in it: owner-only, in the private
    // folder, and deleted as soon as it's sealed.
    let snap = data.join("backup-snapshot.db");
    remove_db(&snap);
    let result = (|| -> Res<PathBuf> {
        {
            let st = lock(store)?;
            st.snapshot_to(&snap).map_err(err)?;
            restrict_to_owner(&snap);
            let copy = Store::open(&snap).map_err(err)?;
            // The interface's own preferences (theme, week start) live in the web view.
            copy.set_secret("ui_prefs", &ui.to_string()).map_err(err)?;
            for k in keyring_keys(&st)? {
                if let Ok(v) = secrets::get(&st, &k) {
                    copy.set_secret(&k, &v).map_err(err)?;
                }
            }
        }
        let name = format!("Carteiro backup {stamp}.carteiro");
        let dest = dest_dir.join(&name);
        let part = dest_dir.join(format!("{name}.part"));
        let sealed = (|| -> Res<()> {
            let mut src = std::io::BufReader::new(std::fs::File::open(&snap).map_err(err)?);
            let mut dst = std::io::BufWriter::new(std::fs::File::create(&part).map_err(err)?);
            restrict_to_owner(&part);
            mail_core::backup::encrypt(&mut src, &mut dst, passphrase).map_err(err)?;
            dst.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(err)
        })();
        if let Err(e) = sealed {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &dest).map_err(err)?;
        Ok(dest)
    })();
    remove_db(&snap);
    result
}

pub struct Restored {
    pub accounts: usize,
    pub ui: serde_json::Value,
}

/// Replaces this install's data with the backup at `data/UPLOAD`. Blocking.
pub fn restore(data: &Path, store: &Mutex<Store>, passphrase: &str) -> Res<Restored> {
    let upload = data.join(UPLOAD);
    let restored = data.join("restore.db");
    remove_db(&restored);
    let opened = (|| -> Res<()> {
        let mut src = std::io::BufReader::new(std::fs::File::open(&upload).map_err(|_| "Choose the backup file again")?);
        let mut dst = std::io::BufWriter::new(std::fs::File::create(&restored).map_err(err)?);
        restrict_to_owner(&restored);
        mail_core::backup::decrypt(&mut src, &mut dst, passphrase).map_err(err)?;
        dst.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(err)?;
        // Must open as a Carteiro database (older ones are upgraded here).
        Store::open(&restored).and_then(|s| s.list_accounts()).map_err(|_| "The backup doesn't contain Carteiro data")?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&upload);
    if let Err(e) = opened {
        remove_db(&restored);
        return Err(e);
    }
    let _ = std::fs::remove_file(data.join("restore.db-wal"));
    let _ = std::fs::remove_file(data.join("restore.db-shm"));

    let db = data.join("mail.db");
    let before = data.join("mail.db.before-restore");
    let mut st = lock(store)?;
    // Close the live database before its files are swapped.
    *st = Store::open_in_memory().map_err(err)?;
    let _ = std::fs::remove_file(data.join("mail.db-wal"));
    let _ = std::fs::remove_file(data.join("mail.db-shm"));
    let _ = std::fs::remove_file(&before);
    if db.exists() {
        std::fs::rename(&db, &before).map_err(err)?;
    }
    match std::fs::rename(&restored, &db).map_err(err).and_then(|_| Store::open(&db).map_err(err)) {
        Ok(new) => *st = new,
        Err(e) => {
            let _ = std::fs::rename(&before, &db);
            *st = Store::open(&db).map_err(err)?;
            return Err(e);
        }
    }
    let _ = std::fs::remove_file(&before);
    for f in ["mail.db", "mail.db-wal", "mail.db-shm"] {
        restrict_to_owner(&data.join(f));
    }
    st.ensure_local_calendar().map_err(err)?;
    // Passwords go back to the system keyring where there is one.
    for (k, v) in st.all_secrets().map_err(err)? {
        if NOT_KEYRING.contains(&k.as_str()) {
            continue;
        }
        secrets::set(&st, &k, &v)?;
        #[cfg(any(windows, target_os = "macos"))]
        st.delete_secret(&k).map_err(err)?;
    }
    let ui = st.get_secret("ui_prefs").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    Ok(Restored { accounts: st.list_accounts().map_err(err)?.len(), ui })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::contacts::Contact;
    use mail_core::{Account, AuthMethod, Endpoint, Protocol, Security};

    fn acct(email: &str) -> Account {
        let ep = Endpoint { host: "mail.example".into(), port: 993, security: Security::Tls, allow_insecure: false };
        Account {
            id: 0,
            name: "N".into(),
            email: email.into(),
            username: email.into(),
            incoming_protocol: Protocol::Imap,
            incoming: ep.clone(),
            smtp: ep,
            smtp_username: None,
            signature: String::new(),
            auth: AuthMethod::Password,
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("carteiro-backup-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One install backs up; another (with its own, different data) restores
    /// and ends up with the first one's data and passwords. Runs the real
    /// secrets code with no keyring reachable, so nothing touches yours.
    #[test]
    fn backup_moves_everything_to_another_install() {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
        let (a, b, out) = (dir("a"), dir("b"), dir("out"));

        let sa = Store::open(&a.join("mail.db")).unwrap();
        let mut acc = acct("me@example.org");
        sa.add_account(&mut acc).unwrap();
        secrets::set(&sa, &secrets::password_key(acc.id), "hunter2 pw").unwrap();
        sa.save_contact(None, &Contact { name: "Ana".into(), emails: vec!["ana@example.org".into()], ..Default::default() }).unwrap();
        sa.set_secret("app_settings", r#"{"sync_minutes":7}"#).unwrap();
        let sa = Mutex::new(sa);
        let ui = serde_json::json!({ "theme": "dark", "week_start": "1" });
        let file = create(&a, &sa, &out, "long passphrase", &ui, "2026-11-08 1600").unwrap();
        assert_eq!(file.file_name().unwrap(), "Carteiro backup 2026-11-08 1600.carteiro");
        assert!(!a.join("backup-snapshot.db").exists(), "the plain copy is removed");
        let sealed = std::fs::read(&file).unwrap();
        assert!(!sealed.windows(10).any(|w| w == b"hunter2 pw"), "passwords never in clear");
        assert!(!sealed.windows(15).any(|w| w == b"ana@example.org"));

        let sb = Store::open(&b.join("mail.db")).unwrap();
        sb.add_account(&mut acct("other@example.org")).unwrap();
        let sb = Mutex::new(sb);

        // A wrong passphrase changes nothing.
        std::fs::copy(&file, b.join(UPLOAD)).unwrap();
        assert!(restore(&b, &sb, "wrong passphrase").is_err());
        assert_eq!(lock(&sb).unwrap().list_accounts().unwrap()[0].email, "other@example.org");

        std::fs::copy(&file, b.join(UPLOAD)).unwrap();
        let r = restore(&b, &sb, "long passphrase").unwrap();
        assert_eq!(r.accounts, 1);
        assert_eq!(r.ui, ui);
        let st = lock(&sb).unwrap();
        let accounts = st.list_accounts().unwrap();
        assert_eq!(accounts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["me@example.org"]);
        assert_eq!(secrets::get(&st, &secrets::password_key(accounts[0].id)).unwrap(), "hunter2 pw");
        assert_eq!(st.list_contacts("", 10).unwrap()[0].contact.name, "Ana");
        assert_eq!(st.get_secret("app_settings").unwrap().as_deref(), Some(r#"{"sync_minutes":7}"#));
        assert!(!b.join(UPLOAD).exists() && !b.join("restore.db").exists() && !b.join("mail.db.before-restore").exists());
        drop(st);
        // And it's still there after a restart.
        let again = Store::open(&b.join("mail.db")).unwrap();
        assert_eq!(again.list_accounts().unwrap()[0].email, "me@example.org");
        for d in [a, b, out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Restores a backup file into a data folder without the interface:
/// CARTEIRO_BACKUP=<file> CARTEIRO_DATA=<folder> CARTEIRO_PASSPHRASE=<…>
/// cargo test -p carteiro --lib -- --ignored restore_into_folder
#[cfg(test)]
#[test]
#[ignore]
fn restore_into_folder() {
    let var = |k| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let data = PathBuf::from(var("CARTEIRO_DATA"));
    std::fs::create_dir_all(&data).unwrap();
    std::fs::copy(var("CARTEIRO_BACKUP"), data.join(UPLOAD)).unwrap();
    let store = Mutex::new(Store::open(&data.join("mail.db")).unwrap());
    let r = restore(&data, &store, &var("CARTEIRO_PASSPHRASE")).unwrap();
    println!("restored {} accounts", r.accounts);
}
