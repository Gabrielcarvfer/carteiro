//! The bundled ClamAV engine: a private `clamd` that only listens on
//! 127.0.0.1, with its own signature database kept fresh by the official
//! `freshclam` (ClamAV's mirrors only serve their own updaters).
//!
//! Binaries come from the app's resources (Windows/macOS/Linux bundles), the
//! APK's native library dir (Android, shipped as libclamd.so/libfreshclam.so so
//! the OS allows executing them), or, for development, the system PATH.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::process::{Child, Command};

const UPDATE_EVERY: Duration = Duration::from_secs(12 * 3600);
const READY_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    /// "missing" | "updating" | "starting" | "ready" | "error"
    pub state: String,
    pub detail: String,
    /// e.g. "ClamAV 1.5.4/28176/Sat Nov  7 08:21:01 2026"
    pub version: Option<String>,
}

pub struct Bins {
    clamd: PathBuf,
    freshclam: PathBuf,
    /// Bundled shared libraries (Linux build), put on LD_LIBRARY_PATH.
    libs: Option<PathBuf>,
    /// Signature-verification certificates shipped with ClamAV ≥ 1.4.
    certs: Option<PathBuf>,
    /// TLS root certificates for freshclam's downloads (Android), as one PEM file.
    ca_bundle: Option<PathBuf>,
}

pub struct Engine {
    dir: PathBuf,
    bins: Option<Bins>,
    child: tokio::sync::Mutex<Option<Child>>,
    addr: Mutex<Option<String>>,
    status: Mutex<Status>,
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Directory holding our own native library on Android (nativeLibraryDir).
/// ClamAV's public signing certificate (from the pinned source release), used
/// to verify signature downloads. Android can't ship a plain certs folder.
#[cfg(target_os = "android")]
const CLAMAV_CERT: &[u8] = include_bytes!("../clamav/clamav.crt");

#[cfg(target_os = "android")]
fn native_lib_dir() -> Option<PathBuf> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .filter_map(|l| l.split_whitespace().last())
        .find(|p| p.ends_with("/libcarteiro_lib.so"))
        .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))
}

fn find_bins(resource_dir: Option<&Path>) -> Option<Bins> {
    #[cfg(target_os = "android")]
    {
        // Android only executes files from the APK's native library folder.
        let _ = resource_dir;
        let dir = native_lib_dir()?;
        let (clamd, freshclam) = (dir.join("libclamd.so"), dir.join("libfreshclam.so"));
        return (clamd.exists() && freshclam.exists()).then_some(Bins { clamd, freshclam, libs: None, certs: None, ca_bundle: None });
    }
    #[allow(unreachable_code)]
    {
        if let Some(res) = resource_dir.map(|r| r.join("clamav")) {
            let (clamd, freshclam) = (res.join(exe("clamd")), res.join(exe("freshclam")));
            if clamd.exists() && freshclam.exists() {
                let certs = Some(res.join("certs")).filter(|c| c.exists());
                let libs = Some(res.join("lib")).filter(|l| l.exists());
                return Some(Bins { clamd, freshclam, libs, certs, ca_bundle: None });
            }
        }
        // Development / Linux distributions: the system's ClamAV.
        let which = |n: &str| {
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
                .chain(["/usr/sbin", "/usr/local/sbin"].map(PathBuf::from))
                .map(|d| d.join(exe(n)))
                .find(|p| p.exists())
        };
        Some(Bins { clamd: which("clamd")?, freshclam: which("freshclam")?, libs: None, certs: None, ca_bundle: None })
    }
}

/// Runs ClamAV from a private copy in the app's data folder rather than from
/// the install/build folder: the originals stay unlocked (app updates and
/// rebuilds can replace them while the engine runs), and the executable bit,
/// which installers don't always keep, is guaranteed. Copies are refreshed
/// whenever the bundled files change.
fn private_copy(b: Bins, dir: &Path) -> Bins {
    // System-wide installs (development) are used in place.
    let Some(src) = b.clamd.parent().map(Path::to_path_buf) else { return b };
    if !src.ends_with("clamav") {
        return b;
    }
    if copy_tree(&src, dir).is_err() {
        return b;
    }
    let name = |p: &Path| dir.join(p.file_name().unwrap_or_default());
    #[cfg(unix)]
    for p in [name(&b.clamd), name(&b.freshclam)] {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
    }
    Bins {
        clamd: name(&b.clamd),
        freshclam: name(&b.freshclam),
        libs: b.libs.map(|_| dir.join("lib")),
        certs: b.certs.map(|_| dir.join("certs")),
        ca_bundle: b.ca_bundle,
    }
}

/// Copies files whose size or modification time differ.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
            continue;
        }
        let (sm, dm) = (entry.metadata()?, std::fs::metadata(&dst).ok());
        let same = dm.is_some_and(|d| d.len() == sm.len() && d.modified().ok() >= sm.modified().ok());
        if !same {
            // Write beside, then rename: never modify a file a running clamd maps.
            let tmp = to.join(format!(".{}.new", entry.file_name().to_string_lossy()));
            std::fs::copy(&src, &tmp)?;
            std::fs::rename(&tmp, &dst)?;
        }
    }
    Ok(())
}

/// freshclam's curl can't use Android's certificate folders directly (they're
/// named by OpenSSL's old hash), so the device's trusted roots are joined
/// into one PEM file, refreshed at every start. The updatable Conscrypt store
/// comes first; older Android versions only have /system.
#[cfg(target_os = "android")]
fn android_ca_bundle(dir: &Path) -> Option<PathBuf> {
    let src = ["/apex/com.android.conscrypt/cacerts", "/system/etc/security/cacerts"]
        .iter()
        .map(Path::new)
        .find(|d| d.is_dir())?;
    let mut pem = String::new();
    for entry in std::fs::read_dir(src).ok()?.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            // Each file is one PEM certificate, sometimes followed by text.
            if let (Some(a), Some(b)) = (text.find("-----BEGIN CERTIFICATE-----"), text.find("-----END CERTIFICATE-----")) {
                pem.push_str(&text[a..b + "-----END CERTIFICATE-----".len()]);
                pem.push('\n');
            }
        }
    }
    let path = dir.join("ca-bundle.pem");
    (!pem.is_empty() && std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, pem)).is_ok()).then_some(path)
}

#[cfg_attr(unix, allow(dead_code))]
fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl Bins {
    fn command(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        if let Some(l) = &self.libs {
            // Bundled libraries take precedence over the build's RUNPATH.
            c.env("LD_LIBRARY_PATH", l);
        }
        if let Some(certs) = &self.certs {
            // Root cert used to verify the signatures' own digital signatures.
            c.env("CVD_CERTS_DIR", certs);
        }
        if let Some(bundle) = &self.ca_bundle {
            c.env("CURL_CA_BUNDLE", bundle);
        }
        c.stdin(Stdio::null()).kill_on_drop(true);
        // If Carteiro dies without cleaning up (killed, out of memory), the
        // kernel stops ClamAV too instead of leaving it holding our files.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            c.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        c
    }
}

impl Engine {
    pub fn new(data_dir: &Path, resource_dir: Option<&Path>) -> Engine {
        let bins = find_bins(resource_dir).map(|b| private_copy(b, &data_dir.join("clamav").join("engine")));
        #[cfg(target_os = "android")]
        let bins = bins.map(|mut b| {
            let certs = data_dir.join("clamav").join("certs");
            if std::fs::create_dir_all(&certs).and_then(|_| std::fs::write(certs.join("clamav.crt"), CLAMAV_CERT)).is_ok() {
                b.certs = Some(certs);
            }
            b.ca_bundle = android_ca_bundle(&data_dir.join("clamav"));
            b
        });
        let status = match &bins {
            Some(_) => Status { state: "starting".into(), ..Default::default() },
            None => Status { state: "missing".into(), detail: "ClamAV isn't included in this build".into(), version: None },
        };
        Engine { dir: data_dir.join("clamav"), bins, child: tokio::sync::Mutex::new(None), addr: Mutex::new(None), status: Mutex::new(status) }
    }

    pub fn addr(&self) -> Option<String> {
        self.addr.lock().ok()?.clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(&self, state: &str, detail: impl Into<String>) {
        if let Ok(mut s) = self.status.lock() {
            s.state = state.into();
            s.detail = detail.into();
        }
    }

    fn db_dir(&self) -> PathBuf {
        self.dir.join("db")
    }

    fn has_db(&self) -> bool {
        std::fs::read_dir(self.db_dir())
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("daily.")))
            .unwrap_or(false)
    }

    /// Runs freshclam once. Returns Ok(true) if signatures changed.
    pub async fn update(&self) -> Result<bool, String> {
        let bins = self.bins.as_ref().ok_or("ClamAV isn't included in this build")?;
        std::fs::create_dir_all(self.db_dir()).map_err(|e| e.to_string())?;
        let conf = self.dir.join("freshclam.conf");
        let text = format!(
            "DatabaseDirectory {}\nDatabaseMirror database.clamav.net\nUpdateLogFile {}\nLogTime yes\nChecks 0\n",
            self.db_dir().display(),
            self.dir.join("freshclam.log").display()
        );
        std::fs::write(&conf, text).map_err(|e| e.to_string())?;
        mail_core::scan::fresh_install_id(&self.db_dir().join("freshclam.dat"));
        let first = !self.has_db();
        if first {
            self.set("updating", "Downloading virus signatures (about 300 MB, first run only)…");
        }
        let out = bins
            .command(&bins.freshclam)
            .arg(format!("--config-file={}", conf.display()))
            .args(bins.certs.as_ref().map(|c| format!("--cvdcertsdir={}", c.display())))
            .arg("--stdout")
            .output()
            .await
            .map_err(|e| format!("couldn't run freshclam: {e}"))?;
        let log = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        // freshclam exits 1 when everything is already up to date.
        if !out.status.success() && out.status.code() != Some(1) {
            let last = log.lines().rev().find(|l| l.contains("ERROR") || l.contains("WARNING")).unwrap_or("update failed").trim().to_string();
            if !self.has_db() {
                return Err(last);
            }
            return Ok(false); // keep using the signatures we have
        }
        Ok(first || log.contains("updated") || log.contains("Downloaded"))
    }

    /// Stops a clamd left behind by an earlier run with our data folder (it
    /// would hold the log and socket). Only a process that is verifiably our
    /// engine with our config is touched.
    async fn stop_stale_clamd(&self, conf: &Path) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let marker = format!("--config-file={}", conf.display());
            let me = std::process::id();
            for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
                if pid as u32 == me {
                    continue;
                }
                let Ok(cmd) = std::fs::read(entry.path().join("cmdline")) else { continue };
                let args: Vec<String> = cmd.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
                let is_clamd = args.first().is_some_and(|a| a.ends_with("clamd") || a.ends_with("libclamd.so"));
                if is_clamd && args.contains(&marker) {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    for _ in 0..50 {
                        if !entry.path().exists() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    if entry.path().exists() {
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = conf;
            let pidfile = self.dir.join("clamd.pid");
            if let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse::<u32>().ok()) {
                let filter = format!("PID eq {pid}");
                let listed = Command::new("tasklist").args(["/FI", &filter, "/FO", "CSV", "/NH"]).output().await;
                if listed.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("clamd.exe")) {
                    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output().await;
                }
            }
            let _ = std::fs::remove_file(pidfile);
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
        let _ = conf;
    }

    async fn start_clamd(&self) -> Result<(), String> {
        let bins = self.bins.as_ref().ok_or("ClamAV isn't included in this build")?;
        // Unix: a socket inside our 0700 folder, reachable only by this user.
        // Windows clamd only speaks TCP, so it listens on localhost.
        let (listen, addr) = if cfg!(unix) {
            let sock = self.dir.join("clamd.sock");
            let _ = std::fs::remove_file(&sock);
            (format!("LocalSocket {}\nLocalSocketMode 600", sock.display()), format!("unix:{}", sock.display()))
        } else {
            let port = free_port().map_err(|e| e.to_string())?;
            (format!("TCPSocket {port}\nTCPAddr 127.0.0.1"), format!("127.0.0.1:{port}"))
        };
        let conf = self.dir.join("clamd.conf");
        std::fs::write(
            &conf,
            format!(
                "DatabaseDirectory {db}\n{listen}\nForeground yes\nLogFile {log}\nLogTime yes\nPidFile {pid}\n\
                 MaxThreads 4\nStreamMaxLength 150M\nMaxScanSize 400M\nMaxFileSize 150M\nMaxRecursion 17\n\
                 AlertBrokenExecutables yes\nAlertEncrypted no\nAlertOLE2Macros no\nPhishingSignatures yes\nPhishingScanURLs yes\n",
                db = self.db_dir().display(),
                log = self.dir.join("clamd.log").display(),
                pid = self.dir.join("clamd.pid").display()
            ),
        )
        .map_err(|e| e.to_string())?;
        self.stop_stale_clamd(&conf).await;
        self.set("starting", "Loading virus signatures…");
        // Startup errors go to stderr, not the log: keep them for the message.
        let errfile = self.dir.join("clamd.stderr");
        let stderr = std::fs::File::create(&errfile).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        let child = bins
            .command(&bins.clamd)
            .arg(format!("--config-file={}", conf.display()))
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("couldn't start clamd: {e}"))?;
        *self.child.lock().await = Some(child);
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(v) = mail_core::scan::probe(&addr).await {
                if let Ok(mut a) = self.addr.lock() {
                    *a = Some(addr.clone());
                }
                if let Ok(mut s) = self.status.lock() {
                    *s = Status { state: "ready".into(), detail: String::new(), version: Some(v) };
                }
                return Ok(());
            }
            if let Some(c) = self.child.lock().await.as_mut() {
                if let Ok(Some(code)) = c.try_wait() {
                    let why = std::fs::read_to_string(&errfile).unwrap_or_default();
                    let last: Vec<&str> = why.lines().filter(|l| !l.trim().is_empty()).collect();
                    let detail = last[last.len().saturating_sub(3)..].join(" ");
                    return Err(if detail.is_empty() {
                        format!("clamd exited during startup ({code}); see {}", self.dir.join("clamd.log").display())
                    } else {
                        format!("clamd exited during startup ({code}): {detail}")
                    });
                }
            }
            if tokio::time::Instant::now() > deadline {
                return Err("clamd took too long to start".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn stop_clamd(&self) {
        if let Ok(mut a) = self.addr.lock() {
            *a = None;
        }
        if let Some(mut c) = self.child.lock().await.take() {
            let _ = c.kill().await;
        }
    }

    /// Keeps the engine running and its signatures fresh. Never returns.
    pub async fn run(&self) {
        if self.bins.is_none() {
            return;
        }
        loop {
            match self.update().await {
                Ok(changed) => {
                    let running = self.addr().is_some();
                    if changed || !running {
                        self.stop_clamd().await;
                        if let Err(e) = self.start_clamd().await {
                            self.set("error", e);
                        }
                    }
                }
                Err(e) if self.addr().is_none() => self.set("error", format!("Couldn't download virus signatures: {e}")),
                Err(_) => {}
            }
            tokio::time::sleep(UPDATE_EVERY).await;
        }
    }

    /// "Update now" from Settings.
    pub async fn update_now(&self) -> Result<Status, String> {
        if self.update().await? || self.addr().is_none() {
            self.stop_clamd().await;
            self.start_clamd().await?;
        }
        Ok(self.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::scan::{scan, ScannerConfig, Verdict};

    /// Real end-to-end run: downloads signatures and starts clamd. Needs ClamAV
    /// installed (or bundled) and network access, so it's opt-in:
    ///   cargo test -p carteiro -- --ignored --nocapture engine_detects_eicar
    #[tokio::test]
    #[ignore]
    async fn engine_detects_eicar() {
        let dir = std::env::temp_dir().join("carteiro-clamav-test");
        let res = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
        let engine = Engine::new(&dir, Some(&res));
        assert!(engine.bins.is_some(), "clamd/freshclam not found");
        let status = engine.update_now().await.expect("engine start");
        println!("{status:?}");
        let cfg = ScannerConfig { clamd_addr: engine.addr().unwrap(), ..Default::default() };
        // The standard antivirus test file, inside an email attachment.
        let eicar = concat!("X5O!P%@AP[4\\PZX54(P^)7CC)7}$", "EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*");
        let mail = format!("Subject: test\r\nContent-Type: text/plain\r\n\r\n{eicar}\r\n");
        let verdict = scan(&cfg, mail.as_bytes()).await;
        println!("{verdict:?}");
        assert!(matches!(verdict, Verdict::Infected { .. }));
        assert_eq!(scan(&cfg, b"Subject: hi\r\n\r\nhello\r\n").await, Verdict::Clean);
        engine.stop_clamd().await;
    }
}
