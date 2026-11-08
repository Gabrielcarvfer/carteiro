//! Transport layer: TCP + rustls, implicit TLS and STARTTLS upgrade, and a
//! small buffered line reader shared by the IMAP, POP3 and SMTP clients.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const IO_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_LINE: u64 = 1 << 20;
const MAX_LITERAL: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Implicit TLS (IMAPS 993, POP3S 995, SMTPS 465).
    Tls,
    /// Plain connect, then mandatory upgrade. Never falls back to plaintext.
    StartTls,
    /// No encryption. Only allowed with `allow_insecure`.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub security: Security,
    #[serde(default)]
    pub allow_insecure: bool,
}

impl Endpoint {
    pub fn check_policy(&self) -> Result<()> {
        if self.security == Security::None && !self.allow_insecure {
            return Err(Error::InsecureRefused);
        }
        Ok(())
    }
}

pub enum MailStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl MailStream {
    pub fn is_tls(&self) -> bool {
        matches!(self, MailStream::Tls(_))
    }
}

impl AsyncRead for MailStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MailStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MailStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MailStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MailStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MailStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Timeout)??;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

/// TLS 1.2+ only, certificates verified against the Mozilla root set,
/// hostname verification always on.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        add_test_ca(&mut roots);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .expect("TLS 1.2/1.3 are supported by ring")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// Debug builds only: trust an extra CA from `CARTEIRO_TEST_CA` (a PEM file),
/// for the local test mail server. Release builds ignore it entirely.
#[cfg(debug_assertions)]
fn add_test_ca(roots: &mut rustls::RootCertStore) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    let Some(path) = std::env::var_os("CARTEIRO_TEST_CA") else { return };
    if let Ok(certs) = CertificateDer::pem_file_iter(&path) {
        for cert in certs.flatten() {
            let _ = roots.add(cert);
        }
    }
}

#[cfg(not(debug_assertions))]
fn add_test_ca(_: &mut rustls::RootCertStore) {}

pub async fn tls_wrap(tcp: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let name = ServerName::try_from(host.to_string()).map_err(|_| Error::InvalidHost(host.into()))?;
    let connector = TlsConnector::from(tls_config());
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
        .await
        .map_err(|_| Error::Timeout)??;
    Ok(tls)
}

/// Buffered connection. Reads are buffered; writes go straight through and
/// are flushed, so a command is always on the wire before we wait for a reply.
pub struct Conn<S> {
    io: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    pub fn new(stream: S) -> Self {
        Self { io: BufReader::new(stream) }
    }

    /// Reads one line, without the trailing CRLF.
    pub async fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut limited = (&mut self.io).take(MAX_LINE);
        let n = tokio::time::timeout(IO_TIMEOUT, limited.read_until(b'\n', &mut buf))
            .await
            .map_err(|_| Error::Timeout)??;
        if n == 0 {
            return Err(Error::Protocol("connection closed by server".into()));
        }
        if buf.last() != Some(&b'\n') {
            return Err(Error::Protocol("line too long or truncated".into()));
        }
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        Ok(buf)
    }

    pub async fn read_exact_n(&mut self, n: usize) -> Result<Vec<u8>> {
        if n > MAX_LITERAL {
            return Err(Error::Protocol(format!("server sent an oversized literal ({n} bytes)")));
        }
        let mut buf = vec![0u8; n];
        tokio::time::timeout(IO_TIMEOUT, self.io.read_exact(&mut buf))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(buf)
    }

    /// Reads until the peer closes the connection, up to `limit` bytes.
    pub async fn read_to_end(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut limited = (&mut self.io).take(limit as u64 + 1);
        match tokio::time::timeout(IO_TIMEOUT, limited.read_to_end(&mut buf)).await.map_err(|_| Error::Timeout)? {
            Ok(_) => {}
            // Servers often close TLS without close_notify after the body.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e.into()),
        }
        if buf.len() > limit {
            return Err(Error::Protocol("response too large".into()));
        }
        Ok(buf)
    }

    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let io = &mut self.io;
        tokio::time::timeout(IO_TIMEOUT, async move {
            io.write_all(data).await?;
            io.flush().await
        })
        .await
        .map_err(|_| Error::Timeout)??;
        Ok(())
    }

    /// Returns the raw stream for a STARTTLS upgrade. Any bytes the server
    /// sent after its "go ahead" would be plaintext injected before the
    /// handshake (CVE-2011-0411 class), so that is a hard error.
    pub fn into_inner_checked(self) -> Result<S> {
        if !self.io.buffer().is_empty() {
            return Err(Error::Protocol("server sent unexpected data before TLS negotiation".into()));
        }
        Ok(self.io.into_inner())
    }
}

pub async fn starttls_upgrade(conn: Conn<MailStream>, host: &str) -> Result<Conn<MailStream>> {
    match conn.into_inner_checked()? {
        MailStream::Plain(tcp) => Ok(Conn::new(MailStream::Tls(Box::new(tls_wrap(tcp, host).await?)))),
        MailStream::Tls(_) => Err(Error::Protocol("connection is already encrypted".into())),
    }
}

/// Rejects anything that could inject an extra protocol line.
pub fn check_line(s: &str) -> Result<()> {
    if s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(Error::Protocol("invalid control character in command".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_requires_opt_in() {
        let mut ep = Endpoint { host: "x".into(), port: 143, security: Security::None, allow_insecure: false };
        assert!(matches!(ep.check_policy(), Err(Error::InsecureRefused)));
        ep.allow_insecure = true;
        assert!(ep.check_policy().is_ok());
        ep.security = Security::StartTls;
        ep.allow_insecure = false;
        assert!(ep.check_policy().is_ok());
    }

    #[tokio::test]
    async fn rejects_data_buffered_before_tls() {
        let (client, mut server) = tokio::io::duplex(1024);
        server.write_all(b"220 go ahead\r\nINJECTED\r\n").await.unwrap();
        let mut conn = Conn::new(client);
        assert_eq!(conn.read_line().await.unwrap(), b"220 go ahead");
        assert!(conn.into_inner_checked().is_err());
    }

    #[test]
    fn check_line_blocks_crlf() {
        assert!(check_line("RCPT TO:<a@b>\r\nDATA").is_err());
        assert!(check_line("NOOP").is_ok());
    }
}
