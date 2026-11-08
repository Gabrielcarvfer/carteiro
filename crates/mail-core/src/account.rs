use serde::{Deserialize, Serialize};

use crate::net::Endpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Imap,
    Pop3,
}

/// How the account signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    #[default]
    Password,
    Google,
    Microsoft,
}

/// What is presented to the server: a password, or an OAuth access token
/// (sent with SASL XOAUTH2).
#[derive(Clone)]
pub enum Credential {
    Password(String),
    Bearer(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Credential::Password(_) => "Credential::Password(..)",
            Credential::Bearer(_) => "Credential::Bearer(..)",
        })
    }
}

/// SASL XOAUTH2 initial response (Google and Microsoft IMAP/SMTP).
pub fn xoauth2(user: &str, token: &str) -> String {
    crate::b64::encode(format!("user={user}\x01auth=Bearer {token}\x01\x01"))
}

/// Account configuration. The password is never stored here; it lives in the
/// OS credential store (see the app's `secrets` module).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub email: String,
    pub username: String,
    pub incoming_protocol: Protocol,
    pub incoming: Endpoint,
    pub smtp: Endpoint,
    /// Defaults to `username` when absent.
    #[serde(default)]
    pub smtp_username: Option<String>,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub auth: AuthMethod,
}

impl Account {
    pub fn smtp_user(&self) -> &str {
        self.smtp_username.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
    }
}
