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

/// freshclam sends a per-install UUID (stored in freshclam.dat) in its
/// User-Agent with every update, letting the update servers link all of a
/// user's updates. ClamAV has no option to turn it off, so a fresh random
/// UUID replaces it before each run; the file's other state (the CDN's
/// rate-limit cooldown) is kept.
pub fn fresh_install_id(dat: &std::path::Path) {
    let Ok(mut data) = std::fs::read(dat) else { return };
    let is_uuid = |w: &[u8]| {
        w.len() == 36
            && w.iter().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { *b == b'-' } else { b.is_ascii_hexdigit() })
    };
    let Some(at) = data.windows(36).position(is_uuid) else { return };
    let mut r = [0u8; 16];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut r).is_err() {
        return;
    }
    r[6] = (r[6] & 0x0f) | 0x40; // version 4
    r[8] = (r[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
    let uuid = format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
    data[at..at + 36].copy_from_slice(uuid.as_bytes());
    let tmp = dat.with_extension("dat.new");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, dat);
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

// ---- remembering results --------------------------------------------------------

static VERSION_CACHE: std::sync::Mutex<Option<(String, Option<String>, i64)>> = std::sync::Mutex::new(None);

/// The signature database version of the running clamd ("28176" from
/// "ClamAV 1.5.4/28176/…"), asked at most every 10 minutes and given 5
/// seconds; unknown (results not remembered) when clamd doesn't say.
async fn signatures_version(cfg: &ScannerConfig) -> Option<String> {
    let now = crate::calendar::now();
    if let Some((addr, v, at)) = VERSION_CACHE.lock().ok()?.clone() {
        if addr == cfg.clamd_addr && now - at < 600 {
            return v;
        }
    }
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), probe(&cfg.clamd_addr)).await.ok().and_then(|r| r.ok());
    let v = reply
        .as_deref()
        .and_then(|r| r.split('/').nth(1))
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string);
    // No answer is asked again within 30 s (clamd may be about to start).
    let at = if v.is_some() { now } else { now - 570 };
    *VERSION_CACHE.lock().ok()? = Some((cfg.clamd_addr.clone(), v.clone(), at));
    v
}

/// `scan`, remembering results by the content's SHA-256: the same bytes are
/// scanned once per signature version (new signatures may catch what old
/// ones missed), and anything found infected stays blocked.
pub async fn scan_cached(store: &std::sync::Mutex<crate::store::Store>, cfg: &ScannerConfig, data: &[u8]) -> Verdict {
    if !cfg.enabled {
        return Verdict::NotScanned { reason: "scanning disabled".into() };
    }
    let hash = crate::update::sha256_hex(data);
    let known = store.lock().ok().and_then(|st| st.scan_result(&hash).ok().flatten());
    if let Some((Some(signature), _)) = &known {
        return Verdict::Infected { signature: signature.clone() };
    }
    // The signature version now, or, while clamd is starting (or the app was
    // just reopened), the last one it reported: content found clean with it
    // needn't wait for clamd.
    let version = match signatures_version(cfg).await {
        Some(v) => {
            if let Ok(st) = store.lock() {
                let _ = st.set_secret("clamav_version", &v);
            }
            Some(v)
        }
        None => store.lock().ok().and_then(|st| st.get_secret("clamav_version").ok().flatten()),
    };
    if let (Some((None, seen)), Some(v)) = (&known, &version) {
        if seen == v {
            return Verdict::Clean;
        }
    }
    let verdict = scan(cfg, data).await;
    if let (Some(v), Ok(st)) = (&version, store.lock()) {
        match &verdict {
            Verdict::Clean => { let _ = st.put_scan_result(&hash, None, v); }
            Verdict::Infected { signature } => { let _ = st.put_scan_result(&hash, Some(signature), v); }
            Verdict::NotScanned { .. } => {}
        }
    }
    verdict
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

#[cfg(test)]
mod install_id_tests {
    #[test]
    fn freshclam_install_id_is_replaced_and_the_rest_kept() {
        let dir = std::env::temp_dir().join(format!("carteiro-dat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dat = dir.join("freshclam.dat");
        let mut orig = vec![1u8, 2, 3, 0];
        orig.extend_from_slice(b"77f2fa98-b811-4390-a602-4d82c30dba9c");
        orig.extend_from_slice(&[0, 9, 9, 9, 42]); // cooldown state etc.
        std::fs::write(&dat, &orig).unwrap();
        super::fresh_install_id(&dat);
        let after = std::fs::read(&dat).unwrap();
        assert_eq!(after.len(), orig.len());
        assert_eq!(&after[..4], &orig[..4]);
        assert_eq!(&after[40..], &orig[40..], "the rest of the state is kept");
        let id = std::str::from_utf8(&after[4..40]).unwrap();
        assert_ne!(id, "77f2fa98-b811-4390-a602-4d82c30dba9c");
        assert_eq!(&id[14..15], "4", "a v4 UUID: {id}");
        super::fresh_install_id(&dir.join("missing.dat")); // no file: nothing to do
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A clamd that answers VERSION with `version` and counts INSTREAM scans,
    /// flagging content that contains "EVIL".
    async fn counting_clamd(version: std::sync::Arc<std::sync::Mutex<String>>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let scans = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = scans.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let (version, counter) = (version.clone(), counter.clone());
                tokio::spawn(async move {
                    // "zVERSION\0" is 9 bytes, "zINSTREAM\0" 10.
                    let mut cmd = vec![0u8; 9];
                    if s.read_exact(&mut cmd).await.is_err() {
                        return;
                    }
                    if cmd != b"zVERSION\0" {
                        let mut last = [0u8; 1];
                        s.read_exact(&mut last).await.unwrap();
                        cmd.push(last[0]);
                    }
                    if cmd == b"zINSTREAM\0" {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let mut data = Vec::new();
                        loop {
                            let mut len = [0u8; 4];
                            s.read_exact(&mut len).await.unwrap();
                            let n = u32::from_be_bytes(len) as usize;
                            if n == 0 {
                                break;
                            }
                            let mut chunk = vec![0u8; n];
                            s.read_exact(&mut chunk).await.unwrap();
                            data.extend(chunk);
                        }
                        let reply: &[u8] = if data.windows(4).any(|w| w == b"EVIL") { b"stream: Test.Evil FOUND\0" } else { b"stream: OK\0" };
                        let _ = s.write_all(reply).await;
                    } else {
                        let reply = format!("ClamAV 1.5.4/{}/Sun Nov  8 2026\0", version.lock().unwrap());
                        let _ = s.write_all(reply.as_bytes()).await;
                    }
                    let _ = s.shutdown().await;
                });
            }
        });
        (addr, scans)
    }

    #[tokio::test]
    async fn same_content_is_scanned_once_per_signature_version() {
        use crate::scan::{scan_cached, ScannerConfig, Verdict};
        use std::sync::atomic::Ordering;
        let version = std::sync::Arc::new(std::sync::Mutex::new("28176".to_string()));
        let (addr, scans) = counting_clamd(version.clone()).await;
        let cfg = ScannerConfig { enabled: true, builtin: false, clamd_addr: addr, block_when_unavailable: true };
        let store = std::sync::Mutex::new(crate::store::Store::open_in_memory().unwrap());
        assert_eq!(scan_cached(&store, &cfg, b"hello").await, Verdict::Clean);
        assert_eq!(scan_cached(&store, &cfg, b"hello").await, Verdict::Clean);
        assert_eq!(scans.load(Ordering::SeqCst), 1, "the second look is remembered");
        assert_eq!(scan_cached(&store, &cfg, b"other").await, Verdict::Clean);
        assert_eq!(scans.load(Ordering::SeqCst), 2, "different content is scanned");
        let evil = Verdict::Infected { signature: "Test.Evil".into() };
        assert_eq!(scan_cached(&store, &cfg, b"an EVIL file").await, evil);
        assert_eq!(scan_cached(&store, &cfg, b"an EVIL file").await, evil);
        assert_eq!(scans.load(Ordering::SeqCst), 3);
        // New signatures: clean content is checked again (once); infected stays blocked.
        *version.lock().unwrap() = "28177".into();
        *crate::scan::VERSION_CACHE.lock().unwrap() = None;
        assert_eq!(scan_cached(&store, &cfg, b"hello").await, Verdict::Clean);
        assert_eq!(scan_cached(&store, &cfg, b"hello").await, Verdict::Clean);
        assert_eq!(scan_cached(&store, &cfg, b"an EVIL file").await, evil);
        assert_eq!(scans.load(Ordering::SeqCst), 4);
        // clamd down (restarting): content clean with the last version opens
        // at once; new content waits.
        let down = ScannerConfig { clamd_addr: "127.0.0.1:1".into(), ..cfg.clone() };
        *crate::scan::VERSION_CACHE.lock().unwrap() = None;
        assert_eq!(scan_cached(&store, &down, b"hello").await, Verdict::Clean, "known clean, no clamd needed");
        assert!(matches!(scan_cached(&store, &down, b"never seen").await, Verdict::NotScanned { .. }));
        assert_eq!(scan_cached(&store, &down, b"an EVIL file").await, evil, "infected stays blocked");
        // Turned off: nothing is claimed.
        let off = ScannerConfig { enabled: false, ..cfg.clone() };
        assert!(matches!(scan_cached(&store, &off, b"hello").await, Verdict::NotScanned { .. }));
    }
}
