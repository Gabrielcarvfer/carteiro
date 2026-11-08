//! Passwords and OAuth refresh tokens. Windows: Credential Manager. macOS:
//! Keychain. Linux: the Secret Service (GNOME Keyring / KWallet) through
//! `secret-tool`, falling back to the app-private database when no keyring
//! is running. Android: the app-private database, which only this app's
//! sandbox can read.
// TODO: wrap the Android value with a key from the Android Keystore.
//
// Keys: "account-<id>" (mail password), "oauth-acct-<id>" / "oauth-src-<id>"
// (refresh tokens), "cal-<id>" (CalDAV password).

use mail_core::store::Store;

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::*;
    const SERVICE: &str = "carteiro";
    /// The app's former names; secrets saved under them are moved on first use.
    const LEGACY_SERVICES: &[&str] = &["vantage", "thunderbonx"];

    fn entry(key: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, key).map_err(|e| e.to_string())
    }
    pub fn get(_: &Store, key: &str) -> Result<String, String> {
        match entry(key)?.get_password() {
            Ok(v) => Ok(v),
            Err(e) => {
                let legacy = LEGACY_SERVICES
                    .iter()
                    .find_map(|svc| keyring::Entry::new(svc, key).and_then(|l| l.get_password()).ok())
                    .ok_or(());
                match legacy {
                    Ok(v) => {
                        let _ = entry(key)?.set_password(&v);
                        Ok(v)
                    }
                    Err(_) => Err(format!("secret not found in credential store: {e}")),
                }
            }
        }
    }
    pub fn set(_: &Store, key: &str, value: &str) -> Result<(), String> {
        entry(key)?.set_password(value).map_err(|e| e.to_string())
    }
    pub fn delete(_: &Store, key: &str) -> Result<(), String> {
        let _ = entry(key)?.delete_credential();
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const SERVICE: &str = "carteiro";

    fn attrs(key: &str) -> [&str; 4] {
        ["service", SERVICE, "key", key]
    }

    /// The secret goes through stdin, never the command line (which other
    /// processes can read).
    fn store(key: &str, value: &str) -> bool {
        let label = format!("Carteiro: {key}");
        let Ok(mut child) = Command::new("secret-tool")
            .args(["store", "--label", &label])
            .args(attrs(key))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let wrote = child.stdin.take().map(|mut i| i.write_all(value.as_bytes()).is_ok()).unwrap_or(false);
        wrote && child.wait().map(|s| s.success()).unwrap_or(false)
    }

    fn lookup(key: &str) -> Option<String> {
        let out = Command::new("secret-tool").arg("lookup").args(attrs(key)).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned()).filter(|v| !v.is_empty())
    }

    pub fn get(st: &Store, key: &str) -> Result<String, String> {
        if let Some(v) = lookup(key) {
            return Ok(v);
        }
        st.get_secret(key).map_err(|e| e.to_string())?.ok_or_else(|| "secret not stored".into())
    }

    pub fn set(st: &Store, key: &str, value: &str) -> Result<(), String> {
        if store(key, value) {
            // Make sure no older copy lingers in the database.
            let _ = st.delete_secret(key);
            return Ok(());
        }
        st.set_secret(key, value).map_err(|e| e.to_string())
    }

    pub fn delete(st: &Store, key: &str) -> Result<(), String> {
        let _ = Command::new("secret-tool").arg("clear").args(attrs(key)).stdin(Stdio::null()).status();
        st.delete_secret(key).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    mod tests {
        /// Touches the real keyring (with a throwaway key), so it's opt-in.
        #[test]
        #[ignore]
        fn secret_tool_roundtrip() {
            let st = mail_core::store::Store::open_in_memory().unwrap();
            let key = "selftest-ignore-me";
            super::set(&st, key, "s3cr\'et\n").unwrap();
            assert_eq!(super::get(&st, key).unwrap(), "s3cr\'et\n");
            assert!(st.get_secret(key).unwrap().is_none(), "stored in the keyring, not the database");
            super::delete(&st, key).unwrap();
            assert!(super::get(&st, key).is_err());
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;
    pub fn get(st: &Store, key: &str) -> Result<String, String> {
        st.get_secret(key).map_err(|e| e.to_string())?.ok_or_else(|| "secret not stored".into())
    }
    pub fn set(st: &Store, key: &str, value: &str) -> Result<(), String> {
        st.set_secret(key, value).map_err(|e| e.to_string())
    }
    pub fn delete(st: &Store, key: &str) -> Result<(), String> {
        st.delete_secret(key).map_err(|e| e.to_string())
    }
}

pub use imp::{delete, get, set};

pub fn password_key(account_id: i64) -> String {
    format!("account-{account_id}")
}
