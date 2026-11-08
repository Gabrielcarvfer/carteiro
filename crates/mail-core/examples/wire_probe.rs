//! Logs in to real servers with a fake password, over every supported
//! transport, so a packet capture can be searched for that password.
//! Used by scripts/wire-check.sh. Never pass a real password here.

use mail_core::account::Credential;
use mail_core::imap::ImapClient;
use mail_core::net::{Endpoint, Security};
use mail_core::pop3::Pop3Client;
use mail_core::smtp::SmtpClient;

const FAKE: &str = "Carteiro-Wire-Test-7f3a9c";

fn ep(host: &str, port: u16, security: Security) -> Endpoint {
    Endpoint { host: host.into(), port, security, allow_insecure: false }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cred = Credential::Password(FAKE.into());
    // Always a made-up user: never send failed logins to a real account.
    let user = "carteiro-wire-test@example.invalid".to_string();
    if std::env::args().len() > 2 {
        eprintln!("note: user names can't be overridden; using {user}");
    }
    let imap_host = std::env::args().nth(1).unwrap_or_else(|| "imap.gmail.com".into());

    for (port, sec) in [(993, Security::Tls), (143, Security::StartTls)] {
        let r = async {
            let mut c = ImapClient::connect(&ep(&imap_host, port, sec)).await?;
            c.authenticate(&user, &cred).await
        }
        .await;
        println!("IMAP {imap_host}:{port} {sec:?}: {}", r.map(|_| "accepted?!".to_string()).unwrap_or_else(|e| e.to_string()));
    }
    for (port, sec) in [(465, Security::Tls), (587, Security::StartTls)] {
        let r = async {
            let mut c = SmtpClient::connect(&ep("smtp.gmail.com", port, sec)).await?;
            c.authenticate(&user, &cred).await
        }
        .await;
        println!("SMTP smtp.gmail.com:{port} {sec:?}: {}", r.map(|_| "accepted?!".to_string()).unwrap_or_else(|e| e.to_string()));
    }
    let r = async {
        let mut c = Pop3Client::connect(&ep("pop.gmail.com", 995, Security::Tls)).await?;
        c.authenticate(&user, &cred).await
    }
    .await;
    println!("POP3 pop.gmail.com:995 Tls: {}", r.map(|_| "accepted?!".to_string()).unwrap_or_else(|e| e.to_string()));
}
