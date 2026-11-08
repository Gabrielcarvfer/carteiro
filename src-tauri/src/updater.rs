//! Self-update from signed GitHub releases (see mail_core::update).
//!
//! - Linux AppImage: the new AppImage replaces the running file; the next
//!   start uses it.
//! - Windows: the MSI is staged and installed when Carteiro quits (or right
//!   away with "Install now").
//! - Android: apps can't replace themselves; the release page opens so the
//!   user installs the APK (Android checks it's signed with the same key).
//!
//! Every package must also pass the ClamAV scan before it's used.

use std::path::PathBuf;
use std::sync::Mutex;

use mail_core::scan::{self, Verdict};
use mail_core::update::{self, Channel, Manifest};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{notify, settings, AppState};

pub fn channel() -> Option<Channel> {
    Channel::from_build(option_env!("CARTEIRO_UPDATE_REPO"), option_env!("CARTEIRO_UPDATE_PUBKEY"))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    /// This build has an update channel.
    pub enabled: bool,
    pub current: String,
    /// A newer, verified release.
    pub available: Option<String>,
    /// Downloaded, verified and scanned; applied on restart / quit.
    pub ready: bool,
    pub message: String,
    #[serde(skip)]
    manifest: Option<Manifest>,
    #[serde(skip)]
    staged: Option<PathBuf>,
}

pub struct Updater {
    status: Mutex<Status>,
    busy: tokio::sync::Mutex<()>,
}

impl Updater {
    pub fn new() -> Updater {
        let enabled = channel().is_some();
        let message = if enabled { String::new() } else { "This build doesn't include an update channel (local build).".into() };
        Updater { status: Mutex::new(Status { enabled, current: env!("CARGO_PKG_VERSION").into(), message, ..Status::default() }), busy: Default::default() }
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(&self, f: impl FnOnce(&mut Status)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }
}

/// Checks for a newer release; with `install`, also downloads and stages it.
pub async fn check(app: &AppHandle, install: bool) -> Result<Status, String> {
    let state = app.state::<AppState>();
    let up = &state.updater;
    let Some(ch) = channel() else { return Ok(up.status()) };
    let _guard = up.busy.lock().await;
    if up.status().ready {
        return Ok(up.status());
    }
    let manifest = match ch.check(env!("CARGO_PKG_VERSION")).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            up.set(|s| {
                s.available = None;
                s.message = "Carteiro is up to date.".into();
            });
            return Ok(up.status());
        }
        Err(e) => {
            up.set(|s| s.message = format!("Couldn't check for updates: {e}"));
            return Err(e.to_string());
        }
    };
    let version = manifest.version.clone();
    up.set(|s| {
        s.available = Some(version.clone());
        s.manifest = Some(manifest.clone());
        s.message = format!("Carteiro {version} is available.");
    });
    if !install || cfg!(target_os = "android") {
        return Ok(up.status());
    }
    match stage(app, &ch, &manifest).await {
        Ok(Some(path)) => {
            up.set(|s| {
                s.ready = true;
                s.staged = Some(path);
                s.message = if cfg!(windows) {
                    format!("Carteiro {version} will be installed when you quit Carteiro.")
                } else {
                    format!("Carteiro {version} is installed and starts next time.")
                };
            });
            if settings(&state).notify {
                notify::show(app, "Carteiro update ready", &up.status().message);
            }
        }
        Ok(None) => {}
        Err(e) => up.set(|s| s.message = format!("Update to {version} failed: {e}")),
    }
    Ok(up.status())
}

/// Downloads, verifies and scans the package, then puts it in place.
/// Ok(None): this installation can't update itself (not an AppImage).
async fn stage(app: &AppHandle, ch: &Channel, m: &Manifest) -> Result<Option<PathBuf>, String> {
    let kind = update::platform_kind().ok_or("updates aren't supported on this platform")?;
    #[cfg(target_os = "linux")]
    let Some(target) = std::env::var_os("APPIMAGE").map(PathBuf::from) else {
        app.state::<AppState>().updater.set(|s| s.message = format!("Carteiro {} is available. Update it with the tool you installed it with.", m.version));
        return Ok(None);
    };
    let (asset, data) = ch.download(m, kind).await.map_err(|e| e.to_string())?;
    let cfg = app.state::<AppState>().scanner.lock().map(|c| c.clone()).unwrap_or_default();
    if let Verdict::Infected { signature } = scan::scan(&cfg, &data).await {
        return Err(format!("the virus scanner flagged the download ({signature}); it was discarded"));
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        // Same folder, then an atomic rename: the running AppImage keeps its
        // already-open file, and the next start uses the new one.
        let tmp = target.with_extension("update");
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("couldn't replace {}: {e}", target.display())
        })?;
        let _ = asset;
        Ok(Some(target))
    }
    #[cfg(windows)]
    {
        let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("updates");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(&asset.name);
        std::fs::write(&path, &data).map_err(|e| e.to_string())?;
        Ok(Some(path))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (app, asset, data);
        Err("updates aren't supported on this platform".into())
    }
}

/// "Install now": Windows runs the staged installer and quits; Linux
/// restarts into the new AppImage; Android opens the release page.
pub fn install_now(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>().updater.status();
    if cfg!(target_os = "android") || !st.ready {
        let (Some(ch), Some(v)) = (channel(), st.available) else { return Err("No update is available.".into()) };
        return crate::open_browser(&ch.release_page(&v));
    }
    #[cfg(target_os = "linux")]
    if let Some(path) = &st.staged {
        std::process::Command::new(path).spawn().map_err(|e| e.to_string())?;
    }
    // Windows: the installer runs from the exit hook below.
    app.exit(0);
    Ok(())
}

/// Called as Carteiro exits: starts a staged Windows installer.
pub fn on_exit(app: &AppHandle) {
    #[cfg(windows)]
    if let Some(msi) = app.state::<AppState>().updater.status().staged {
        // Passive: a progress bar, no questions; Windows asks for permission.
        let _ = std::process::Command::new("msiexec").arg("/i").arg(&msi).args(["/passive", "/norestart"]).spawn();
    }
    let _ = app;
}

/// Background checks: shortly after start, then twice a day.
pub fn spawn_background(app: AppHandle) {
    if channel().is_none() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            let auto = settings(&app.state::<AppState>()).auto_update;
            let before = app.state::<AppState>().updater.status().available;
            if let Ok(st) = check(&app, auto).await {
                // Android (or auto-install off): tell the user once per version.
                if !st.ready && st.available.is_some() && st.available != before && settings(&app.state::<AppState>()).notify {
                    notify::show(&app, "Carteiro update available", &st.message);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(12 * 3600)).await;
        }
    });
}
