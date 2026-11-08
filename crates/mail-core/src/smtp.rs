//! SMTP submission client (RFC 5321, 3207 STARTTLS, 4954 AUTH).

use crate::b64::encode as b64;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::account::{xoauth2, Credential};
use crate::error::{Error, Result};
use crate::net::{self, Conn, Endpoint, MailStream, Security};

pub struct SmtpClient<S> {
    conn: Conn<S>,
    pub extensions: Vec<String>,
}

impl SmtpClient<MailStream> {
    /// Connects, greets and negotiates encryption. STARTTLS is mandatory when
    /// requested: if the server doesn't offer it we fail rather than continue
    /// in plaintext.
    pub async fn connect(ep: &Endpoint) -> Result<Self> {
        ep.check_policy()?;
        let tcp = net::connect_tcp(&ep.host, ep.port).await?;
        let mut c = match ep.security {
            Security::Tls => SmtpClient::new(MailStream::Tls(Box::new(net::tls_wrap(tcp, &ep.host).await?))),
            Security::StartTls | Security::None => SmtpClient::new(MailStream::Plain(tcp)),
        };
        c.greeting().await?;
        c.ehlo().await?;
        if ep.security == Security::StartTls {
            if !c.has_ext("STARTTLS") {
                return Err(Error::Protocol("server does not offer STARTTLS".into()));
            }
            c.expect(Some("STARTTLS"), &[220]).await?;
            let conn = net::starttls_upgrade(c.conn, &ep.host).await?;
            c = SmtpClient { conn, extensions: Vec::new() };
            c.ehlo().await?;
        }
        Ok(c)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> SmtpClient<S> {
    pub fn new(stream: S) -> Self {
        Self { conn: Conn::new(stream), extensions: Vec::new() }
    }

    async fn reply(&mut self) -> Result<(u16, Vec<String>)> {
        let mut lines = Vec::new();
        loop {
            let line = self.conn.read_line().await?;
            let code: u16 = line
                .get(..3)
                .and_then(|c| std::str::from_utf8(c).ok())
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| Error::Protocol(format!("bad SMTP reply: {}", String::from_utf8_lossy(&line))))?;
            lines.push(String::from_utf8_lossy(line.get(4..).unwrap_or(&[])).into_owned());
            if line.get(3) != Some(&b'-') {
                return Ok((code, lines));
            }
            if lines.len() > 1000 {
                return Err(Error::Protocol("SMTP reply too long".into()));
            }
        }
    }

    async fn expect(&mut self, cmd: Option<&str>, ok: &[u16]) -> Result<Vec<String>> {
        if let Some(c) = cmd {
            net::check_line(c)?;
            self.conn.write_all(format!("{c}\r\n").as_bytes()).await?;
        }
        let (code, lines) = self.reply().await?;
        if ok.contains(&code) {
            Ok(lines)
        } else {
            Err(Error::Server(format!("{code} {}", lines.join(" "))))
        }
    }

    pub async fn greeting(&mut self) -> Result<()> {
        self.expect(None, &[220]).await.map(|_| ())
    }

    pub async fn ehlo(&mut self) -> Result<()> {
        let lines = self.expect(Some("EHLO [127.0.0.1]"), &[250]).await?;
        self.extensions = lines.into_iter().skip(1).map(|l| l.to_ascii_uppercase()).collect();
        Ok(())
    }

    pub fn has_ext(&self, name: &str) -> bool {
        self.extensions.iter().any(|e| e.split_whitespace().next() == Some(name))
    }

    fn auth_mechs(&self) -> Vec<String> {
        self.extensions
            .iter()
            .filter_map(|e| e.strip_prefix("AUTH ").or_else(|| e.strip_prefix("AUTH=")))
            .flat_map(|m| m.split_whitespace().map(str::to_string))
            .collect()
    }

    pub async fn auth(&mut self, user: &str, pass: &str) -> Result<()> {
        let mechs = self.auth_mechs();
        let auth_err = |e: Error| Error::Auth(e.to_string());
        if mechs.iter().any(|m| m == "PLAIN") {
            let t = b64(format!("\0{user}\0{pass}"));
            self.expect(Some(&format!("AUTH PLAIN {t}")), &[235]).await.map_err(auth_err)?;
        } else if mechs.iter().any(|m| m == "LOGIN") {
            self.expect(Some("AUTH LOGIN"), &[334]).await.map_err(auth_err)?;
            self.expect(Some(&b64(user)), &[334]).await.map_err(auth_err)?;
            self.expect(Some(&b64(pass)), &[235]).await.map_err(auth_err)?;
        } else {
            return Err(Error::Auth("server offers no supported authentication mechanism".into()));
        }
        Ok(())
    }

    pub async fn authenticate(&mut self, user: &str, cred: &Credential) -> Result<()> {
        match cred {
            Credential::Password(p) => self.auth(user, p).await,
            Credential::Bearer(token) => self
                .expect(Some(&format!("AUTH XOAUTH2 {}", xoauth2(user, token))), &[235])
                .await
                .map(|_| ())
                .map_err(|e| Error::Auth(e.to_string())),
        }
    }

    pub async fn send(&mut self, from: &str, rcpts: &[String], data: &[u8]) -> Result<()> {
        check_addr(from)?;
        self.expect(Some(&format!("MAIL FROM:<{from}>")), &[250]).await?;
        for r in rcpts {
            check_addr(r)?;
            self.expect(Some(&format!("RCPT TO:<{r}>")), &[250, 251]).await?;
        }
        self.expect(Some("DATA"), &[354]).await?;
        self.conn.write_all(&dot_stuff(data)).await?;
        self.expect(None, &[250]).await.map(|_| ())
    }

    pub async fn quit(mut self) {
        let _ = self.expect(Some("QUIT"), &[221]).await;
    }
}

fn check_addr(a: &str) -> Result<()> {
    if a.is_empty() || a.bytes().any(|b| b <= b' ' || b == b'<' || b == b'>' || b == 0x7f) {
        return Err(Error::Other(format!("invalid email address: {a}")));
    }
    Ok(())
}

/// Normalizes line endings to CRLF, escapes leading dots and appends the
/// terminating `.` line.
pub fn dot_stuff(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    let mut parts: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    if parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    for p in parts {
        let p = p.strip_suffix(b"\r").unwrap_or(p);
        if p.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// Opens a session, authenticates and sends one message.
pub async fn send_mail(ep: &Endpoint, user: &str, cred: &Credential, from: &str, rcpts: &[String], data: &[u8]) -> Result<()> {
    let mut c = SmtpClient::connect(ep).await?;
    if !user.is_empty() {
        c.authenticate(user, cred).await?;
    }
    c.send(from, rcpts, data).await?;
    c.quit().await;
    Ok(())
}

pub async fn verify(ep: &Endpoint, user: &str, cred: &Credential) -> Result<()> {
    let mut c = SmtpClient::connect(ep).await?;
    if !user.is_empty() {
        c.authenticate(user, cred).await?;
    }
    c.quit().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn dot_stuffing() {
        assert_eq!(dot_stuff(b"a\n.b\r\n..c"), b"a\r\n..b\r\n...c\r\n.\r\n");
        assert_eq!(dot_stuff(b"x\r\n"), b"x\r\n.\r\n");
    }

    #[tokio::test]
    async fn auth_and_send() {
        let (client, server) = tokio::io::duplex(1 << 14);
        let srv = tokio::spawn(async move {
            let mut io = BufReader::new(server);
            io.get_mut().write_all(b"220 mx ready\r\n").await.unwrap();
            let token = b64("\0me\0pw");
            for (want, reply) in [
                ("EHLO [127.0.0.1]".to_string(), &b"250-mx\r\n250-AUTH LOGIN PLAIN\r\n250 8BITMIME\r\n"[..]),
                (format!("AUTH PLAIN {token}"), b"235 ok\r\n"),
                ("MAIL FROM:<me@x.org>".into(), b"250 ok\r\n"),
                ("RCPT TO:<you@y.org>".into(), b"250 ok\r\n"),
                ("DATA".into(), b"354 go\r\n"),
            ] {
                let mut line = String::new();
                io.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), want);
                io.get_mut().write_all(reply).await.unwrap();
            }
            let mut data = vec![0u8; b"Subject: s\r\n\r\n..hi\r\n.\r\n".len()];
            io.read_exact(&mut data).await.unwrap();
            assert_eq!(data, b"Subject: s\r\n\r\n..hi\r\n.\r\n");
            io.get_mut().write_all(b"250 queued\r\n").await.unwrap();
        });
        let mut c = SmtpClient::new(client);
        c.greeting().await.unwrap();
        c.ehlo().await.unwrap();
        assert!(c.has_ext("8BITMIME"));
        c.auth("me", "pw").await.unwrap();
        c.send("me@x.org", &["you@y.org".into()], b"Subject: s\n\n.hi\n").await.unwrap();
        srv.await.unwrap();
    }

    #[test]
    fn rejects_address_injection() {
        assert!(check_addr("a@b>\r\nRCPT TO:<c@d").is_err());
        assert!(check_addr("a@b.c").is_ok());
    }
}
