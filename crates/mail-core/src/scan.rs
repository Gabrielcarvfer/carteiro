//! Malware scanning via ClamAV's `clamd` daemon (INSTREAM protocol).
//! The whole raw message is scanned, which covers HTML bodies and every
//! attachment (clamd decodes MIME itself).

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verdict {
    Clean,
    Infected { signature: String },
    /// No scanner configured or reachable.
    NotScanned { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannerConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Use the ClamAV engine bundled with the app (desktop). When false,
    /// `clamd_addr` points at an existing clamd.
    #[serde(default = "yes")]
    pub builtin: bool,
    /// clamd address: "127.0.0.1:3310" (TCP) or "unix:/path/clamd.sock".
    /// Empty = not ready yet.
    #[serde(default)]
    pub clamd_addr: String,
    /// Messages that could not be scanned get no formatted content or
    /// attachments (plain text stays readable).
    #[serde(default = "yes")]
    pub block_when_unavailable: bool,
}

fn yes() -> bool {
    true
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self { enabled: true, builtin: true, clamd_addr: String::new(), block_when_unavailable: true }
    }
}

const CHUNK: usize = 64 * 1024;

pub async fn scan(cfg: &ScannerConfig, data: &[u8]) -> Verdict {
    if !cfg.enabled {
        return Verdict::NotScanned { reason: "scanning disabled".into() };
    }
    if cfg.clamd_addr.is_empty() {
        return Verdict::NotScanned { reason: "the virus scanner is still starting".into() };
    }
    let result = match connect(&cfg.clamd_addr).await {
        Ok(Clamd::Tcp(s)) => instream(s, data).await,
        #[cfg(unix)]
        Ok(Clamd::Unix(s)) => instream(s, data).await,
        Err(e) => return Verdict::NotScanned { reason: format!("clamd unreachable: {e}") },
    };
    result.unwrap_or_else(|e| Verdict::NotScanned { reason: e.to_string() })
}

enum Clamd {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

/// "unix:/path" uses a Unix socket (private to the user when the socket lives
/// in a 0700 folder, so no other local program can impersonate clamd).
async fn connect(addr: &str) -> Result<Clamd> {
    let timeout = crate::net::CONNECT_TIMEOUT;
    if let Some(path) = addr.strip_prefix("unix:") {
        #[cfg(unix)]
        return Ok(Clamd::Unix(tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path)).await.map_err(|_| Error::Timeout)??));
        #[cfg(not(unix))]
        return Err(Error::Other(format!("Unix sockets aren't available here: {path}")));
    }
    Ok(Clamd::Tcp(tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await.map_err(|_| Error::Timeout)??))
}

pub async fn instream<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, data: &[u8]) -> Result<Verdict> {
    s.write_all(b"zINSTREAM\0").await?;
    for chunk in data.chunks(CHUNK) {
        s.write_all(&(chunk.len() as u32).to_be_bytes()).await?;
        s.write_all(chunk).await?;
    }
    s.write_all(&[0, 0, 0, 0]).await?;
    s.flush().await?;
    let mut reply = Vec::new();
    tokio::time::timeout(crate::net::IO_TIMEOUT, s.read_to_end(&mut reply)).await.map_err(|_| Error::Timeout)??;
    let text = String::from_utf8_lossy(&reply).trim_end_matches(['\0', '\n']).to_string();
    // "stream: OK" | "stream: Eicar-Signature FOUND" | "INSTREAM size limit exceeded. ERROR"
    if text.ends_with(" OK") {
        Ok(Verdict::Clean)
    } else if let Some(sig) = text.strip_suffix(" FOUND") {
        Ok(Verdict::Infected { signature: sig.trim_start_matches("stream:").trim().to_string() })
    } else {
        Err(Error::Other(format!("clamd: {text}")))
    }
}

/// Checks that clamd is reachable and returns its version line, e.g.
/// "ClamAV 1.4.1/27463/Fri Nov  6 08:21:01 2026" (engine/signatures/date).
pub async fn probe(addr: &str) -> Result<String> {
    match connect(addr).await? {
        Clamd::Tcp(s) => version(s).await,
        #[cfg(unix)]
        Clamd::Unix(s) => version(s).await,
    }
}

pub async fn version<S: AsyncRead + AsyncWrite + Unpin>(mut s: S) -> Result<String> {
    s.write_all(b"zVERSION\0").await?;
    s.flush().await?;
    let mut reply = Vec::new();
    tokio::time::timeout(crate::net::IO_TIMEOUT, s.read_to_end(&mut reply)).await.map_err(|_| Error::Timeout)??;
    let text = String::from_utf8_lossy(&reply).trim_end_matches(['\0', '\n']).to_string();
    if !text.starts_with("ClamAV") {
        return Err(Error::Other(format!("unexpected reply from clamd: {text}")));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fake_clamd(reply: &'static [u8]) -> Verdict {
        let (client, mut server) = tokio::io::duplex(1 << 20);
        let srv = tokio::spawn(async move {
            let mut cmd = [0u8; 10];
            server.read_exact(&mut cmd).await.unwrap();
            assert_eq!(&cmd, b"zINSTREAM\0");
            loop {
                let mut len = [0u8; 4];
                server.read_exact(&mut len).await.unwrap();
                let n = u32::from_be_bytes(len) as usize;
                if n == 0 { break; }
                let mut buf = vec![0u8; n];
                server.read_exact(&mut buf).await.unwrap();
            }
            server.write_all(reply).await.unwrap();
        });
        let v = instream(client, &vec![b'x'; 200_000]).await.unwrap();
        srv.await.unwrap();
        v
    }

    #[tokio::test]
    async fn parses_verdicts() {
        assert_eq!(fake_clamd(b"stream: OK\0").await, Verdict::Clean);
        assert_eq!(
            fake_clamd(b"stream: Eicar-Test-Signature FOUND\0").await,
            Verdict::Infected { signature: "Eicar-Test-Signature".into() }
        );
    }

    #[tokio::test]
    async fn version_probe() {
        let (client, mut server) = tokio::io::duplex(1024);
        let srv = tokio::spawn(async move {
            let mut cmd = [0u8; 9];
            server.read_exact(&mut cmd).await.unwrap();
            assert_eq!(&cmd, b"zVERSION\0");
            server.write_all(b"ClamAV 1.4.1/27463/Fri Nov  6 08:21:01 2026\0").await.unwrap();
        });
        assert!(version(client).await.unwrap().starts_with("ClamAV 1.4.1"));
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn disabled_is_not_scanned() {
        let off = ScannerConfig { enabled: false, ..Default::default() };
        assert!(matches!(scan(&off, b"x").await, Verdict::NotScanned { .. }));
        // Enabled but the engine isn't up yet: not scanned (and blocked by default).
        let cfg = ScannerConfig::default();
        assert!(cfg.block_when_unavailable);
        assert!(matches!(scan(&cfg, b"x").await, Verdict::NotScanned { .. }));
    }
}
