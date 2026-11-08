//! POP3 client (RFC 1939 + STLS from RFC 2595).

use tokio::io::{AsyncRead, AsyncWrite};

use crate::account::Credential;
use crate::error::{Error, Result};
use crate::net::{self, Conn, Endpoint, MailStream, Security};

const MAX_MESSAGE: usize = 64 << 20;

pub struct Pop3Client<S> {
    conn: Conn<S>,
    pub capabilities: Vec<String>,
}

impl Pop3Client<MailStream> {
    pub async fn connect(ep: &Endpoint) -> Result<Self> {
        ep.check_policy()?;
        let tcp = net::connect_tcp(&ep.host, ep.port).await?;
        match ep.security {
            Security::Tls => {
                let tls = net::tls_wrap(tcp, &ep.host).await?;
                let mut c = Pop3Client::new(MailStream::Tls(Box::new(tls)));
                c.greeting().await?;
                Ok(c)
            }
            Security::StartTls => {
                let mut c = Pop3Client::new(MailStream::Plain(tcp));
                c.greeting().await?;
                c.cmd("STLS").await?;
                let conn = net::starttls_upgrade(c.conn, &ep.host).await?;
                Ok(Pop3Client { conn, capabilities: Vec::new() })
            }
            Security::None => {
                let mut c = Pop3Client::new(MailStream::Plain(tcp));
                c.greeting().await?;
                Ok(c)
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Pop3Client<S> {
    pub fn new(stream: S) -> Self {
        Self { conn: Conn::new(stream), capabilities: Vec::new() }
    }

    async fn status(&mut self) -> Result<String> {
        let line = self.conn.read_line().await?;
        let s = String::from_utf8_lossy(&line).into_owned();
        match s.strip_prefix("+OK") {
            Some(rest) => Ok(rest.trim().to_string()),
            None => Err(Error::Server(s)),
        }
    }

    async fn cmd(&mut self, c: &str) -> Result<String> {
        net::check_line(c)?;
        self.conn.write_all(format!("{c}\r\n").as_bytes()).await?;
        self.status().await
    }

    async fn multiline(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let line = self.conn.read_line().await?;
            if line == b"." {
                return Ok(out);
            }
            let l = if line.starts_with(b"..") { &line[1..] } else { &line[..] };
            out.extend_from_slice(l);
            out.extend_from_slice(b"\r\n");
            if out.len() > MAX_MESSAGE {
                return Err(Error::Protocol("message too large".into()));
            }
        }
    }

    pub async fn greeting(&mut self) -> Result<()> {
        self.status().await.map(|_| ())
    }

    pub async fn capa(&mut self) -> Result<Vec<String>> {
        match self.cmd("CAPA").await {
            Ok(_) => {
                let b = self.multiline().await?;
                self.capabilities = String::from_utf8_lossy(&b)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
            }
            Err(Error::Server(_)) => self.capabilities.clear(),
            Err(e) => return Err(e),
        }
        Ok(self.capabilities.clone())
    }

    pub async fn login(&mut self, user: &str, pass: &str) -> Result<()> {
        self.cmd(&format!("USER {user}")).await.map_err(|e| Error::Auth(e.to_string()))?;
        // Don't echo the server's reply verbatim in case it reflects input.
        self.cmd(&format!("PASS {pass}")).await.map_err(|_| Error::Auth("invalid user name or password".into()))?;
        Ok(())
    }

    pub async fn authenticate(&mut self, user: &str, cred: &Credential) -> Result<()> {
        match cred {
            Credential::Password(p) => self.login(user, p).await,
            Credential::Bearer(token) => {
                let ir = crate::account::xoauth2(user, token);
                self.cmd(&format!("AUTH XOAUTH2 {ir}")).await.map_err(|e| Error::Auth(e.to_string()))?;
                Ok(())
            }
        }
    }

    pub async fn stat(&mut self) -> Result<(u32, u64)> {
        let s = self.cmd("STAT").await?;
        let mut it = s.split_whitespace();
        let n = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let size = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        Ok((n, size))
    }

    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>> {
        self.cmd("UIDL").await?;
        let body = self.multiline().await?;
        Ok(String::from_utf8_lossy(&body)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.to_string()))
            })
            .collect())
    }

    pub async fn retr(&mut self, n: u32) -> Result<Vec<u8>> {
        self.cmd(&format!("RETR {n}")).await?;
        self.multiline().await
    }

    pub async fn dele(&mut self, n: u32) -> Result<()> {
        self.cmd(&format!("DELE {n}")).await.map(|_| ())
    }

    pub async fn quit(mut self) -> Result<()> {
        self.cmd("QUIT").await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn session() {
        let (client, server) = tokio::io::duplex(1 << 14);
        let srv = tokio::spawn(async move {
            let mut io = BufReader::new(server);
            io.get_mut().write_all(b"+OK ready\r\n").await.unwrap();
            for (want, reply) in [
                ("USER bob", &b"+OK\r\n"[..]),
                ("PASS secret", b"+OK logged in\r\n"),
                ("UIDL", b"+OK\r\n1 abc\r\n2 def\r\n.\r\n"),
                ("RETR 2", b"+OK\r\nSubject: x\r\n\r\n..leading dot\r\nbody\r\n.\r\n"),
                ("QUIT", b"+OK bye\r\n"),
            ] {
                let mut line = String::new();
                io.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end(), want);
                io.get_mut().write_all(reply).await.unwrap();
            }
        });
        let mut c = Pop3Client::new(client);
        c.greeting().await.unwrap();
        c.login("bob", "secret").await.unwrap();
        assert_eq!(c.uidl().await.unwrap(), vec![(1, "abc".into()), (2, "def".into())]);
        let msg = c.retr(2).await.unwrap();
        assert_eq!(msg, b"Subject: x\r\n\r\n.leading dot\r\nbody\r\n");
        c.quit().await.unwrap();
        srv.await.unwrap();
    }
}
