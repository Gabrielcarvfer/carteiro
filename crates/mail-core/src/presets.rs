//! Server settings for well-known providers, with a sane guess otherwise.

use serde::Serialize;

use crate::account::Protocol;
use crate::net::{Endpoint, Security};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Preset {
    pub incoming_protocol: Protocol,
    pub incoming: Endpoint,
    pub smtp: Endpoint,
    pub note: Option<String>,
}

fn ep(host: &str, port: u16, security: Security) -> Endpoint {
    Endpoint { host: host.into(), port, security, allow_insecure: false }
}

pub fn guess(email: &str) -> Preset {
    let domain = email.rsplit('@').next().unwrap_or("").trim().to_lowercase();
    let p = |imap: &str, smtp: &str, smtp_port: u16, smtp_sec: Security, note: Option<&str>| Preset {
        incoming_protocol: Protocol::Imap,
        incoming: ep(imap, 993, Security::Tls),
        smtp: ep(smtp, smtp_port, smtp_sec),
        note: note.map(str::to_string),
    };
    match domain.as_str() {
        "gmail.com" | "googlemail.com" => p("imap.gmail.com", "smtp.gmail.com", 465, Security::Tls, Some("Gmail requires an App Password (Google Account → Security → App passwords).")),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => p("outlook.office365.com", "smtp-mail.outlook.com", 587, Security::StartTls, Some("Microsoft personal accounts now require OAuth sign-in; password login may be rejected.")),
        "yahoo.com" | "ymail.com" => p("imap.mail.yahoo.com", "smtp.mail.yahoo.com", 465, Security::Tls, Some("Yahoo requires an app password.")),
        "icloud.com" | "me.com" | "mac.com" => p("imap.mail.me.com", "smtp.mail.me.com", 587, Security::StartTls, Some("iCloud requires an app-specific password.")),
        "fastmail.com" | "fastmail.fm" => p("imap.fastmail.com", "smtp.fastmail.com", 465, Security::Tls, Some("Fastmail requires an app password.")),
        "gmx.com" | "gmx.net" | "gmx.de" => p("imap.gmx.com", "mail.gmx.com", 587, Security::StartTls, None),
        "zoho.com" => p("imap.zoho.com", "smtp.zoho.com", 465, Security::Tls, None),
        "proton.me" | "protonmail.com" => Preset {
            incoming_protocol: Protocol::Imap,
            incoming: ep("127.0.0.1", 1143, Security::StartTls),
            smtp: ep("127.0.0.1", 1025, Security::StartTls),
            note: Some("Proton requires the Proton Mail Bridge running locally.".into()),
        },
        _ => p(&format!("imap.{domain}"), &format!("smtp.{domain}"), 465, Security::Tls, None),
    }
}
