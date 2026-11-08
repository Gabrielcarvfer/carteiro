//! Wire-level checks: fake plaintext servers record every byte the client
//! sends on the raw TCP socket (what Wireshark would see). The password must
//! never appear, in clear or base64, unless the user explicitly chose an
//! unencrypted connection.

use std::sync::{Arc, Mutex};

use mail_core::account::Credential;
use mail_core::http::{self, Request};
use mail_core::imap::ImapClient;
use mail_core::net::{Endpoint, Security};
use mail_core::pop3::Pop3Client;
use mail_core::smtp::SmtpClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PASSWORD: &str = "Sup3r-Secret-pw!";

/// Starts a one-connection server that sends `greeting`, then answers each
/// client line with `reply_to(line)`. Returns its port and the captured bytes.
async fn recording_server(greeting: &'static str, reply_to: fn(&str) -> String) -> (u16, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let wire = Arc::new(Mutex::new(Vec::new()));
    let captured = wire.clone();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let _ = sock.write_all(greeting.as_bytes()).await;
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        loop {
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            captured.lock().unwrap().extend_from_slice(&buf[..n]);
            line.extend_from_slice(&buf[..n]);
            while let Some(i) = line.iter().position(|&b| b == b'\n') {
                let l: Vec<u8> = line.drain(..=i).collect();
                let reply = reply_to(String::from_utf8_lossy(&l).trim_end());
                if sock.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    });
    (port, wire)
}

fn leaked(wire: &Arc<Mutex<Vec<u8>>>) -> bool {
    let bytes = wire.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes);
    let b64_forms = [
        mail_core::b64::encode(PASSWORD),
        mail_core::b64::encode(format!("\0user\0{PASSWORD}")),
        mail_core::b64::encode(format!("user:{PASSWORD}")),
        mail_core::account::xoauth2("user", PASSWORD),
    ];
    text.contains(PASSWORD) || b64_forms.iter().any(|f| text.contains(f.as_str()))
}

fn ep(port: u16, security: Security) -> Endpoint {
    Endpoint { host: "127.0.0.1".into(), port, security, allow_insecure: false }
}

async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

// ---- STARTTLS stripping: the server (or an attacker in the middle) refuses
// the upgrade. The client must stop, not fall back to plaintext login. ----

#[tokio::test]
async fn imap_starttls_refused_sends_no_credentials() {
    let (port, wire) = recording_server("* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] hi\r\n", |l| {
        let tag = l.split(' ').next().unwrap_or("*");
        format!("{tag} BAD STARTTLS not supported\r\n")
    })
    .await;
    assert!(ImapClient::connect(&ep(port, Security::StartTls)).await.is_err());
    settle().await;
    assert!(!leaked(&wire));
}

#[tokio::test]
async fn smtp_without_starttls_capability_sends_no_credentials() {
    let (port, wire) = recording_server("220 mx\r\n", |l| {
        if l.starts_with("EHLO") { "250-mx\r\n250 AUTH PLAIN LOGIN\r\n".into() } else { "502 no\r\n".into() }
    })
    .await;
    assert!(SmtpClient::connect(&ep(port, Security::StartTls)).await.is_err());
    settle().await;
    assert!(!leaked(&wire));
    let sent = String::from_utf8_lossy(&wire.lock().unwrap()).into_owned();
    assert!(!sent.contains("AUTH"), "no AUTH command may be sent: {sent}");
}

#[tokio::test]
async fn smtp_starttls_rejected_sends_no_credentials() {
    let (port, wire) = recording_server("220 mx\r\n", |l| {
        if l.starts_with("EHLO") { "250-mx\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n".into() } else { "454 TLS not available\r\n".into() }
    })
    .await;
    assert!(SmtpClient::connect(&ep(port, Security::StartTls)).await.is_err());
    settle().await;
    assert!(!leaked(&wire));
}

#[tokio::test]
async fn pop3_stls_refused_sends_no_credentials() {
    let (port, wire) = recording_server("+OK ready\r\n", |_| "-ERR no STLS\r\n".into()).await;
    assert!(Pop3Client::connect(&ep(port, Security::StartTls)).await.is_err());
    settle().await;
    assert!(!leaked(&wire));
}

// ---- Implicit TLS pointed at a plaintext server: handshake fails first. ----

#[tokio::test]
async fn implicit_tls_to_plaintext_server_fails_before_login() {
    for _ in 0..1 {
        let (port, wire) = recording_server("* OK plaintext imap\r\n", |_| "* BAD\r\n".into()).await;
        assert!(ImapClient::connect(&ep(port, Security::Tls)).await.is_err());
        settle().await;
        assert!(!leaked(&wire));
        // What was sent must be a TLS ClientHello, not IMAP text.
        let sent = wire.lock().unwrap().clone();
        assert_eq!(sent.first(), Some(&0x16), "expected a TLS handshake record");
    }
}

// ---- "None" security without the explicit opt-in: refuses to connect. ----

#[tokio::test]
async fn plaintext_requires_explicit_opt_in() {
    let (port, wire) = recording_server("* OK hi\r\n", |_| "* BAD\r\n".into()).await;
    assert!(ImapClient::connect(&ep(port, Security::None)).await.is_err());
    assert!(SmtpClient::connect(&ep(port, Security::None)).await.is_err());
    assert!(Pop3Client::connect(&ep(port, Security::None)).await.is_err());
    settle().await;
    assert!(wire.lock().unwrap().is_empty(), "nothing may even be sent");
}

// ---- Full login through each client after a proper TLS refusal path is
// covered above; here, a successful *explicit* opt-in still works, proving
// the recorder would catch a leak if one happened. ----

#[tokio::test]
async fn recorder_detects_plaintext_when_user_opts_in() {
    let (port, wire) = recording_server("* OK [CAPABILITY IMAP4rev1] hi\r\n", |l| {
        let tag = l.split(' ').next().unwrap_or("*");
        format!("{tag} OK\r\n")
    })
    .await;
    let mut e = ep(port, Security::None);
    e.allow_insecure = true;
    let mut c = ImapClient::connect(&e).await.unwrap();
    c.authenticate("user", &Credential::Password(PASSWORD.into())).await.unwrap();
    settle().await;
    assert!(leaked(&wire), "sanity check: the recorder must see an opted-in plaintext login");
}

// ---- HTTP (CalDAV, Graph, OAuth) ----

#[tokio::test]
async fn http_refuses_plaintext_and_downgrade_redirects() {
    // Plain http:// to anything but localhost is refused before connecting.
    assert!(http::send(Request::new("GET", "http://example.com/").basic("user", PASSWORD)).await.is_err());

    // An HTTPS server can't redirect credentials to plain HTTP; simulated by
    // a local plaintext server redirecting to another origin: the request
    // with credentials must not follow it.
    let (port, wire) = recording_server("", |_| String::new()).await;
    let target = format!("http://127.0.0.1:{port}/steal");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rport = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await;
        let resp = format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\n\r\n");
        let _ = s.write_all(resp.as_bytes()).await;
    });
    let r = http::send(Request::new("GET", &format!("http://localhost:{rport}/")).basic("user", PASSWORD)).await.unwrap();
    assert_eq!(r.status, 302, "cross-origin redirect with credentials is not followed");
    settle().await;
    assert!(!leaked(&wire));
}
