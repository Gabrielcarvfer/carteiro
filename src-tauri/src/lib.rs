mod clamav;
mod notify;
mod secrets;
mod updater;

use std::sync::{Arc, Mutex};

use mail_core::ops::{self, Draft, Flag, MessageView};
use mail_core::presets::{self, Preset};
use mail_core::rules::Rule;
use mail_core::scan::ScannerConfig;
use mail_core::store::{FolderInfo, Store, ThreadSummary};
use mail_core::sync::{self, SyncReport};
use mail_core::calendar;
use mail_core::ical::Event;
use mail_core::oauth::{self, ClientConfig, Purpose};
use mail_core::store::{CalSource, CalendarInfo, StoredEvent};
use mail_core::{Account, AuthMethod, Credential, Endpoint, Protocol, Security};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

pub struct AppState {
    store: Arc<Mutex<Store>>,
    scanner: Mutex<ScannerConfig>,
    settings: Mutex<AppSettings>,
    syncing: AtomicBool,
    /// Unix time of the last offline-copy pass (hourly in the background).
    last_offline: std::sync::atomic::AtomicI64,
    /// Short-lived OAuth access tokens: key -> (token, expires_at).
    tokens: Mutex<HashMap<String, (String, i64)>>,
    clam: Arc<clamav::Engine>,
    updater: updater::Updater,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Closing the window hides it to the tray; mail keeps syncing.
    close_to_tray: bool,
    /// Desktop notification for new Inbox mail.
    notify: bool,
    /// Background sync interval.
    sync_minutes: u64,
    /// OAuth app registrations (client IDs are public for desktop apps).
    google: ClientConfig,
    microsoft: ClientConfig,
    /// Keep message bodies of the last N days on this device (0 = all mail).
    offline_days: u32,
    /// Download and install verified updates in the background.
    auto_update: bool,
}

fn build_client(id: Option<&str>, secret: Option<&str>) -> ClientConfig {
    ClientConfig { client_id: id.unwrap_or("").to_string(), client_secret: secret.map(str::to_string) }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            notify: true,
            sync_minutes: 3,
            // Baked in at build time from CI secrets / env vars, if provided.
            google: build_client(option_env!("CARTEIRO_GOOGLE_CLIENT_ID"), option_env!("CARTEIRO_GOOGLE_CLIENT_SECRET")),
            microsoft: build_client(option_env!("CARTEIRO_MICROSOFT_CLIENT_ID"), None),
            offline_days: 30,
            auto_update: true,
        }
    }
}

fn settings(s: &AppState) -> AppSettings {
    s.settings.lock().map(|c| c.clone()).unwrap_or_default()
}

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn lock(s: &AppState) -> Res<std::sync::MutexGuard<'_, Store>> {
    s.store.lock().map_err(|_| "database lock poisoned".to_string())
}

fn client_for(s: &AppState, provider: AuthMethod) -> ClientConfig {
    let cfg = settings(s);
    match provider {
        AuthMethod::Google => cfg.google,
        _ => cfg.microsoft,
    }
}

/// A valid access token for an OAuth login, refreshed when it has expired.
async fn oauth_token(s: &AppState, provider: AuthMethod, secret_key: &str, purpose: Purpose) -> Res<Credential> {
    let cache_key = format!("{secret_key}-{purpose:?}");
    let now = calendar::now();
    if let Some((t, exp)) = s.tokens.lock().map_err(|_| "lock poisoned")?.get(&cache_key).cloned() {
        if exp > now {
            return Ok(Credential::Bearer(t));
        }
    }
    let refresh = secrets::get(&*lock(s)?, secret_key).map_err(|_| "Signed out: sign in again from Accounts".to_string())?;
    let t = oauth::refresh(provider, &client_for(s, provider), &refresh, purpose).await.map_err(err)?;
    if let Some(rotated) = &t.refresh_token {
        secrets::set(&*lock(s)?, secret_key, rotated)?;
    }
    s.tokens.lock().map_err(|_| "lock poisoned")?.insert(cache_key, (t.access_token.clone(), t.expires_at));
    Ok(Credential::Bearer(t.access_token))
}

/// The account and what to log in with (password or fresh OAuth token).
async fn credential(s: &AppState, id: i64) -> Res<(Account, Credential)> {
    let acct = lock(s)?.get_account(id).map_err(err)?.ok_or("account not found")?;
    let cred = match acct.auth {
        AuthMethod::Password => Credential::Password(secrets::get(&*lock(s)?, &secrets::password_key(id))?),
        p => oauth_token(s, p, &format!("oauth-acct-{id}"), Purpose::Mail).await?,
    };
    Ok((acct, cred))
}

/// Credential for a calendar source (None for the local calendar).
async fn source_credential(s: &AppState, src: &CalSource) -> Res<Option<Credential>> {
    Ok(match src.kind.as_str() {
        "local" | "holidays" | "ics" => None,
        "caldav" => Some(Credential::Password(secrets::get(&*lock(s)?, &format!("cal-{}", src.id))?)),
        kind => {
            let provider = if kind == "google" { AuthMethod::Google } else { AuthMethod::Microsoft };
            let key = match src.account_id {
                Some(a) => format!("oauth-acct-{a}"),
                None => format!("oauth-src-{}", src.id),
            };
            Some(oauth_token(s, provider, &key, Purpose::Calendar).await?)
        }
    })
}

/// The scanner settings with the address of the engine actually in use.
fn scanner(s: &AppState) -> ScannerConfig {
    let mut cfg = s.scanner.lock().map(|c| c.clone()).unwrap_or_default();
    if cfg.builtin {
        cfg.clamd_addr = s.clam.addr().unwrap_or_default();
    }
    cfg
}

#[tauri::command]
fn list_accounts(state: State<'_, AppState>) -> Res<Vec<Account>> {
    lock(&state)?.list_accounts().map_err(err)
}

#[tauri::command]
fn guess_settings(email: String) -> Preset {
    presets::guess(&email)
}

#[tauri::command]
async fn add_account(state: State<'_, AppState>, account: Account, password: String, verify: bool) -> Res<Account> {
    let cred = Credential::Password(password.clone());
    let mut account = if verify { sync::verify_account(&account, &cred).await.map_err(err)? } else { account };
    let st = lock(&state)?;
    st.add_account(&mut account).map_err(err)?;
    if let Err(e) = secrets::set(&st, &secrets::password_key(account.id), &password) {
        let _ = st.delete_account(account.id);
        return Err(e);
    }
    Ok(account)
}

/// Updates display name, signature and SMTP user name. A new password is
/// verified against the servers before it is stored.
#[tauri::command]
async fn update_account(state: State<'_, AppState>, account: Account, password: Option<String>) -> Res<Account> {
    let s = state.inner();
    let mut account = account;
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        account = sync::verify_account(&account, &Credential::Password(pw.clone())).await.map_err(err)?;
        secrets::set(&*lock(s)?, &secrets::password_key(account.id), &pw)?;
    }
    lock(s)?.update_account(&account).map_err(err)?;
    Ok(account)
}

#[tauri::command]
fn remove_account(state: State<'_, AppState>, account_id: i64) -> Res<()> {
    let st = lock(&state)?;
    secrets::delete(&st, &secrets::password_key(account_id))?;
    secrets::delete(&st, &format!("oauth-acct-{account_id}"))?;
    st.delete_account(account_id).map_err(err)
}

#[tauri::command]
async fn sync_account(state: State<'_, AppState>, account_id: i64) -> Res<SyncReport> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    sync::sync_account(&s.store, &acct, &pw).await.map_err(err)
}

#[tauri::command]
fn list_folders(state: State<'_, AppState>, account_id: i64) -> Res<Vec<FolderInfo>> {
    lock(&state)?.list_folders(account_id).map_err(err)
}

#[tauri::command]
fn list_threads(state: State<'_, AppState>, account_id: i64, folder: String, query: Option<String>, offset: u32) -> Res<Vec<ThreadSummary>> {
    lock(&state)?.list_threads(account_id, &folder, query.as_deref(), 100, offset).map_err(err)
}

#[derive(serde::Serialize)]
struct UnifiedThread {
    #[serde(flatten)]
    thread: ThreadSummary,
    account_id: i64,
    account_email: String,
    folder: String,
}

/// Every account's inbox merged into one list, newest first.
#[tauri::command]
fn list_unified(state: State<'_, AppState>, query: Option<String>) -> Res<Vec<UnifiedThread>> {
    let st = lock(&state)?;
    let mut out = Vec::new();
    for a in st.list_accounts().map_err(err)? {
        let Some(inbox) = st.folder_by_special(a.id, "inbox").map_err(err)? else { continue };
        for t in st.list_threads(a.id, &inbox.name, query.as_deref(), 100, 0).map_err(err)? {
            out.push(UnifiedThread { thread: t, account_id: a.id, account_email: a.email.clone(), folder: inbox.name.clone() });
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.thread.last_date));
    out.truncate(150);
    Ok(out)
}

#[tauri::command]
async fn open_thread(state: State<'_, AppState>, account_id: i64, thread_key: String) -> Res<Vec<MessageView>> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::open_thread(&s.store, &acct, &pw, &thread_key, &scanner(s)).await.map_err(err)
}

#[tauri::command]
async fn set_flag(state: State<'_, AppState>, account_id: i64, ids: Vec<i64>, flag: Flag, on: bool) -> Res<()> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::set_flag(&s.store, &acct, &pw, &ids, flag, on).await.map_err(err)
}

#[tauri::command]
async fn move_messages(state: State<'_, AppState>, account_id: i64, ids: Vec<i64>, folder: String) -> Res<()> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::move_messages(&s.store, &acct, &pw, &ids, &folder).await.map_err(err)
}

#[tauri::command]
async fn delete_messages(state: State<'_, AppState>, account_id: i64, ids: Vec<i64>) -> Res<()> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::delete_messages(&s.store, &acct, &pw, &ids).await.map_err(err)
}

#[tauri::command]
async fn send_message(state: State<'_, AppState>, account_id: i64, draft: Draft) -> Res<ops::SendResult> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::send(&s.store, &acct, &pw, &draft).await.map_err(err)
}

#[tauri::command]
fn list_rules(state: State<'_, AppState>) -> Res<Vec<Rule>> {
    lock(&state)?.list_rules().map_err(err)
}

#[tauri::command]
fn save_rule(state: State<'_, AppState>, rule: Rule) -> Res<Rule> {
    let mut rule = rule;
    lock(&state)?.save_rule(&mut rule).map_err(err)?;
    Ok(rule)
}

#[tauri::command]
fn delete_rule(state: State<'_, AppState>, id: i64) -> Res<()> {
    lock(&state)?.delete_rule(id).map_err(err)
}

#[tauri::command]
fn reorder_rules(state: State<'_, AppState>, ids: Vec<i64>) -> Res<()> {
    lock(&state)?.reorder_rules(&ids).map_err(err)
}

#[tauri::command]
async fn run_rules(state: State<'_, AppState>, account_id: i64, folder: String) -> Res<usize> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    ops::run_rules_on_folder(&s.store, &acct, &pw, &folder).await.map_err(err)
}

#[tauri::command]
fn get_scanner(state: State<'_, AppState>) -> ScannerConfig {
    state.scanner.lock().map(|c| c.clone()).unwrap_or_default()
}

#[tauri::command]
fn scanner_status(state: State<'_, AppState>) -> clamav::Status {
    state.clam.status()
}

#[tauri::command]
async fn scanner_update_now(state: State<'_, AppState>) -> Res<clamav::Status> {
    state.clam.update_now().await
}

// ---- tracker list (AdGuard's disguised mail trackers, downloaded) -----------------

#[derive(Serialize)]
struct TrackerStatus {
    builtin: usize,
    downloaded: usize,
    /// Unix time of the last successful check, 0 if never.
    updated: i64,
}

fn tracker_files(app: &AppHandle) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dir = app.path().app_data_dir().ok()?.join("trackers");
    Some((dir.join("mail-trackers.txt"), dir.join("mail-trackers.etag")))
}

fn tracker_status_of(app: &AppHandle) -> TrackerStatus {
    let updated = tracker_files(app)
        .and_then(|(_, etag)| std::fs::metadata(etag).ok()?.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    TrackerStatus { builtin: mail_core::tracking::builtin_count(), downloaded: mail_core::tracking::downloaded_count(), updated }
}

/// Loads the saved list (at start).
fn load_trackers(app: &AppHandle) {
    let Some((list, _)) = tracker_files(app) else { return };
    if let Ok(text) = std::fs::read_to_string(&list) {
        if let Ok(parsed) = mail_core::tracking::parse_host_list(&text) {
            mail_core::tracking::set_downloaded(parsed);
        }
    }
}

/// Downloads the list if it changed (ETag); a rejected download keeps the old copy.
async fn update_trackers_inner(app: &AppHandle) -> Res<()> {
    let (list, etag_file) = tracker_files(app).ok_or("no data folder")?;
    let etag = std::fs::read_to_string(&etag_file).ok().filter(|_| list.exists());
    let Some((text, new_etag)) = mail_core::tracking::download_mail_trackers(etag.as_deref()).await.map_err(err)? else {
        // Unchanged: just record that we checked.
        return std::fs::write(&etag_file, etag.unwrap_or_default()).map_err(err);
    };
    let parsed = mail_core::tracking::parse_host_list(&text).map_err(err)?;
    let dir = list.parent().ok_or("no data folder")?;
    std::fs::create_dir_all(dir).map_err(err)?;
    restrict_to_owner(dir);
    let tmp = list.with_extension("tmp");
    std::fs::write(&tmp, &text).map_err(err)?;
    restrict_to_owner(&tmp);
    std::fs::rename(&tmp, &list).map_err(err)?;
    std::fs::write(&etag_file, new_etag.unwrap_or_default()).map_err(err)?;
    restrict_to_owner(&etag_file);
    mail_core::tracking::set_downloaded(parsed);
    Ok(())
}

#[tauri::command]
fn tracker_status(app: AppHandle) -> TrackerStatus {
    tracker_status_of(&app)
}

#[tauri::command]
async fn update_trackers(app: AppHandle) -> Res<TrackerStatus> {
    update_trackers_inner(&app).await?;
    Ok(tracker_status_of(&app))
}

#[tauri::command]
fn update_status(state: State<'_, AppState>) -> updater::Status {
    state.updater.status()
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Res<updater::Status> {
    updater::check(&app, settings(&app.state::<AppState>()).auto_update).await
}

#[tauri::command]
fn install_update(app: AppHandle) -> Res<()> {
    updater::install_now(&app)
}

#[tauri::command]
fn set_scanner(state: State<'_, AppState>, config: ScannerConfig) -> Res<()> {
    let json = serde_json::to_string(&config).map_err(err)?;
    lock(&state)?.set_secret("scanner_config", &json).map_err(err)?;
    *state.scanner.lock().map_err(|_| "lock poisoned")? = config;
    Ok(())
}

#[tauri::command]
async fn test_scanner(addr: String) -> Res<String> {
    mail_core::scan::probe(&addr).await.map_err(|e| format!("Couldn't reach clamd at {addr}: {e}"))
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> AppSettings {
    settings(&state)
}

#[tauri::command]
fn set_settings(state: State<'_, AppState>, config: AppSettings) -> Res<()> {
    let mut config = config;
    config.sync_minutes = config.sync_minutes.clamp(1, 120);
    lock(&state)?.set_secret("app_settings", &serde_json::to_string(&config).map_err(err)?).map_err(err)?;
    *state.settings.lock().map_err(|_| "lock poisoned")? = config;
    Ok(())
}

/// Syncs every account, notifies about new Inbox mail and tells the UI to
/// refresh. Skips if a background sync is already running.
async fn sync_all(app: &AppHandle) {
    let s = app.state::<AppState>();
    if s.syncing.swap(true, Ordering::SeqCst) {
        return;
    }
    let accounts = lock(&s).and_then(|st| st.list_accounts().map_err(err)).unwrap_or_default();
    let mut arrived = Vec::new();
    for a in &accounts {
        let Ok((_, pw)) = credential(&s, a.id).await else { continue };
        if let Ok(r) = sync::sync_account(&s.store, a, &pw).await {
            arrived.extend(r.arrived);
        }
    }
    let _ = sync_calendars_inner(&s).await;
    // Keep the chosen period available offline, at most hourly.
    if calendar::now() - s.last_offline.load(Ordering::SeqCst) > 3600 {
        s.last_offline.store(calendar::now(), Ordering::SeqCst);
        let _ = offline_all(&s).await;
    }
    // Content search covers only mail the virus scanner found clean.
    let _ = ops::index_clean_text(&s.store, &scanner(&s)).await;
    s.syncing.store(false, Ordering::SeqCst);
    if !arrived.is_empty() && settings(&s).notify {
        let (title, body) = match arrived.as_slice() {
            [one] => (one.from.clone(), if one.subject.is_empty() { "(no subject)".to_string() } else { one.subject.clone() }),
            many => (format!("{} new messages", many.len()), many.iter().take(3).map(|m| m.from.as_str()).collect::<Vec<_>>().join(", ")),
        };
        notify::show(app, &title, &body);
    }
    let _ = app.emit("mail-synced", arrived.len());
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        #[cfg(desktop)]
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_web_and_mail_links_open() {
        for ok in ["https://example.com/a?b=c", "http://x.org", "mailto:a@b.c", "HTTPS://EXAMPLE.COM"] {
            assert!(super::is_openable(ok), "{ok}");
        }
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "ms-settings:", "smb://host/share", "https://a b", "https://x\u{202e}.com", "\\\\host\\share", "C:\\Windows\\calc.exe"] {
            assert!(!super::is_openable(bad), "{bad}");
        }
    }
}

#[cfg(desktop)]
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let open = MenuItem::with_id(app, "open", "Open Carteiro", true, None::<&str>)?;
    let sync = MenuItem::with_id(app, "sync", "Sync now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &sync, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main").tooltip("Carteiro").menu(&menu).show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.on_menu_event(|app, e| match e.id.as_ref() {
        "open" => show_main(app),
        "sync" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { sync_all(&app).await });
        }
        "quit" => app.exit(0),
        _ => {}
    })
    .on_tray_icon_event(|tray, e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show_main(tray.app_handle());
        }
    })
    .build(app)?;
    Ok(())
}

// ---- sign in with Google / Microsoft -------------------------------------------

/// Mail, calendars and (where no keyring exists) secrets live in the app's
/// data folder: on Unix, only the owner may read it (Windows profiles are
/// already private to the user).
fn restrict_to_owner(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = if meta.is_dir() { 0o700 } else { 0o600 };
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Exported mail stays private to the user, wherever it's saved.
fn restrict_tree(path: &std::path::Path) {
    restrict_to_owner(path);
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            restrict_tree(&e.path());
        }
    }
}

/// The system browser, for sign-in. Android can't start it from Rust: the page
/// opens it through MainActivity's link bridge (same checks as open_browser).
fn open_external(app: &AppHandle, url: &str) -> Res<()> {
    #[cfg(target_os = "android")]
    {
        if !is_openable(url) {
            return Err("Only web and email links can be opened".into());
        }
        app.emit("open-url", url).map_err(err)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        open_browser(url)
    }
}

/// Opens a URL in the system browser without an extra plugin.
fn open_browser(url: &str) -> Res<()> {
    if !is_openable(url) {
        return Err("Only web and email links can be opened".into());
    }
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    let r: std::io::Result<()> = Err(std::io::Error::other("opening links isn't supported on this platform yet"));
    r.map(|_| ()).map_err(|e| format!("Couldn't open the browser: {e}"))
}

/// Links from mail may only open web pages or compose an email; never local
/// files, custom URL handlers or anything with hidden control characters.
fn is_openable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    url.len() < 8192
        && !url.chars().any(|c| {
            // Invisible direction/zero-width characters can disguise targets.
            c.is_control() || c.is_whitespace() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        })
        && (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
}

/// A link clicked in a message: opened in the system browser.
#[tauri::command]
fn open_link(url: String) -> Res<()> {
    open_browser(&url)
}

/// The window may only show Carteiro's own pages. Anything else (a link that
/// slipped through, a redirect) is refused, and web links go to the browser.
fn navigation_guard<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("navigation-guard")
        .on_navigation(|_, url| {
            let own = match url.scheme() {
                "tauri" | "asset" | "about" | "data" | "blob" => true,
                "http" | "https" => url.host_str() == Some("tauri.localhost") || (cfg!(dev) && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
                _ => false,
            };
            if !own {
                let _ = open_browser(url.as_str());
            }
            own
        })
        .build()
}

fn oauth_account(provider: AuthMethod, email: &str, name: &str) -> Account {
    let ep = |host: &str, port, security| Endpoint { host: host.into(), port, security, allow_insecure: false };
    let (imap, smtp) = match provider {
        AuthMethod::Google => (ep("imap.gmail.com", 993, Security::Tls), ep("smtp.gmail.com", 465, Security::Tls)),
        _ => (ep("outlook.office365.com", 993, Security::Tls), ep("smtp.office365.com", 587, Security::StartTls)),
    };
    Account {
        id: 0,
        name: name.to_string(),
        email: email.to_string(),
        username: email.to_string(),
        incoming_protocol: Protocol::Imap,
        incoming: imap,
        smtp,
        smtp_username: None,
        signature: String::new(),
        auth: provider,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum SignInResult {
    Account { account: Account },
    Calendar { source_id: i64 },
}

/// `what`: "account" (mail + calendar) or "calendar" (calendar only, e.g. a
/// Microsoft 365 work calendar next to an IMAP mailbox).
#[tauri::command]
async fn oauth_sign_in(app: AppHandle, state: State<'_, AppState>, provider: AuthMethod, what: String) -> Res<SignInResult> {
    let s = state.inner();
    let pending = oauth::begin(provider, &client_for(s, provider), None).await.map_err(err)?;
    open_external(&app, &pending.url)?;
    let tokens = pending.finish().await.map_err(err)?;
    let refresh = tokens.refresh_token.clone().ok_or("The provider didn't return a refresh token")?;
    let email = tokens.email.clone().ok_or("The provider didn't return your email address")?;
    let name = tokens.name.clone().unwrap_or_else(|| email.split('@').next().unwrap_or("").to_string());

    if what == "calendar" {
        let (kind, url) = match provider {
            AuthMethod::Google => ("google", calendar::google_caldav_url(&email)),
            _ => ("microsoft", String::new()),
        };
        let st = lock(s)?;
        let id = st.add_cal_source(kind, None, &url, &email, &email).map_err(err)?;
        secrets::set(&st, &format!("oauth-src-{id}"), &refresh)?;
        return Ok(SignInResult::Calendar { source_id: id });
    }

    let account = oauth_account(provider, &email, &name);
    // Prove the mail token works before saving anything.
    let mut account = sync::verify_account(&account, &Credential::Bearer(tokens.access_token.clone())).await.map_err(err)?;
    let st = lock(s)?;
    st.add_account(&mut account).map_err(err)?;
    secrets::set(&st, &format!("oauth-acct-{}", account.id), &refresh)?;
    let (kind, url) = match provider {
        AuthMethod::Google => ("google", calendar::google_caldav_url(&email)),
        _ => ("microsoft", String::new()),
    };
    st.add_cal_source(kind, Some(account.id), &url, &email, &email).map_err(err)?;
    Ok(SignInResult::Account { account })
}

// ---- calendar -------------------------------------------------------------------------

#[tauri::command]
fn list_calendars(state: State<'_, AppState>) -> Res<Vec<CalendarInfo>> {
    lock(&state)?.calendars().map_err(err)
}

#[tauri::command]
fn list_cal_sources(state: State<'_, AppState>) -> Res<Vec<CalSource>> {
    lock(&state)?.cal_sources().map_err(err)
}

#[tauri::command]
fn calendar_events(state: State<'_, AppState>, from: i64, to: i64) -> Res<Vec<StoredEvent>> {
    lock(&state)?.events_between(from, to).map_err(err)
}

#[tauri::command]
fn set_calendar_visible(state: State<'_, AppState>, calendar_id: i64, visible: bool) -> Res<()> {
    lock(&state)?.set_calendar_visible(calendar_id, visible).map_err(err)
}

async fn sync_calendars_inner(s: &AppState) -> Vec<String> {
    let sources = lock(s).and_then(|st| st.cal_sources().map_err(err)).unwrap_or_default();
    let mut errors = Vec::new();
    for src in sources.iter().filter(|x| x.kind != "local") {
        let r = match source_credential(s, src).await {
            Ok(cred) => calendar::sync_source(&s.store, src, cred.as_ref(), &scanner(s)).await.map_err(err),
            Err(e) => Err(e),
        };
        if let Err(e) = r {
            errors.push(format!("{}: {e}", src.name));
        }
    }
    errors
}

/// Returns per-source errors (empty when everything synced).
#[tauri::command]
async fn sync_calendars(state: State<'_, AppState>) -> Res<Vec<String>> {
    Ok(sync_calendars_inner(state.inner()).await)
}

#[tauri::command]
async fn add_caldav_source(state: State<'_, AppState>, url: String, username: String, password: String, name: String) -> Res<i64> {
    let s = state.inner();
    let cred = Credential::Password(password.clone());
    // Check the server and credentials before saving.
    mail_core::caldav::CalDav { user: username.clone(), cred }.discover(&url).await.map_err(err)?;
    let st = lock(s)?;
    let id = st.add_cal_source("caldav", None, &url, &username, if name.is_empty() { &url } else { &name }).map_err(err)?;
    secrets::set(&st, &format!("cal-{id}"), &password)?;
    Ok(id)
}

#[tauri::command]
async fn holiday_countries() -> Res<Vec<calendar::Country>> {
    calendar::holiday_countries().await.map_err(err)
}

#[tauri::command]
async fn holiday_regions(country: String) -> Res<Vec<String>> {
    calendar::holiday_regions(&country).await.map_err(err)
}

/// Adds a source that needs no sign-in and syncs it right away.
async fn add_open_source(s: &AppState, kind: &str, url: &str, name: &str) -> Res<i64> {
    let id = lock(s)?.add_cal_source(kind, None, url, "", name).map_err(err)?;
    let src = lock(s)?.cal_source(id).map_err(err)?.ok_or("source vanished")?;
    if let Err(e) = calendar::sync_source(&s.store, &src, None, &scanner(s)).await {
        let _ = lock(s)?.delete_cal_source(id);
        return Err(err(e));
    }
    Ok(id)
}

#[tauri::command]
async fn add_holiday_calendar(state: State<'_, AppState>, country: String, region: Option<String>, name: String) -> Res<i64> {
    let url = match region.filter(|r| !r.is_empty()) {
        Some(r) => format!("{country}/{r}"),
        None => country,
    };
    add_open_source(state.inner(), "holidays", &url, &name).await
}

#[tauri::command]
async fn add_ics_calendar(state: State<'_, AppState>, url: String, name: String) -> Res<i64> {
    let url = calendar::normalize_ics_url(&url);
    let name = if name.trim().is_empty() { url.clone() } else { name };
    add_open_source(state.inner(), "ics", &url, &name).await
}

#[tauri::command]
fn add_local_calendar(state: State<'_, AppState>, name: String, color: String) -> Res<i64> {
    let st = lock(&state)?;
    let local = st.ensure_local_calendar().map_err(err)?;
    let src = st.calendar(local).map_err(err)?.ok_or("local calendar missing")?.source_id;
    let remote = format!("local-{}", calendar::now());
    st.upsert_calendar(src, &remote, &name, Some(&color), false).map_err(err)
}

#[tauri::command]
fn remove_local_calendar(state: State<'_, AppState>, calendar_id: i64) -> Res<()> {
    lock(&state)?.delete_local_calendar(calendar_id).map_err(err)
}

#[tauri::command]
fn remove_cal_source(state: State<'_, AppState>, source_id: i64) -> Res<()> {
    let st = lock(&state)?;
    secrets::delete(&st, &format!("cal-{source_id}"))?;
    secrets::delete(&st, &format!("oauth-src-{source_id}"))?;
    st.delete_cal_source(source_id).map_err(err)
}

async fn source_of_calendar(s: &AppState, calendar_id: i64) -> Res<(CalSource, Option<Credential>)> {
    let src = {
        let st = lock(s)?;
        let cal = st.calendar(calendar_id).map_err(err)?.ok_or("calendar not found")?;
        st.cal_source(cal.source_id).map_err(err)?.ok_or("calendar source not found")?
    };
    let cred = source_credential(s, &src).await?;
    Ok((src, cred))
}

#[tauri::command]
async fn save_event(state: State<'_, AppState>, calendar_id: i64, event_id: Option<i64>, event: Event) -> Res<i64> {
    let s = state.inner();
    // Moving an event to another calendar = delete there, create here.
    if let Some(id) = event_id {
        let old_cal = lock(s)?.event(id).map_err(err)?.map(|e| e.calendar_id);
        if let Some(old) = old_cal.filter(|c| *c != calendar_id) {
            let (src, cred) = source_of_calendar(s, old).await?;
            calendar::delete_event(&s.store, &src, cred.as_ref(), id).await.map_err(err)?;
            let (src, cred) = source_of_calendar(s, calendar_id).await?;
            return calendar::save_event(&s.store, &src, cred.as_ref(), calendar_id, None, event).await.map_err(err);
        }
    }
    let (src, cred) = source_of_calendar(s, calendar_id).await?;
    calendar::save_event(&s.store, &src, cred.as_ref(), calendar_id, event_id, event).await.map_err(err)
}

#[tauri::command]
async fn delete_event(state: State<'_, AppState>, event_id: i64) -> Res<()> {
    let s = state.inner();
    let cal = lock(s)?.event(event_id).map_err(err)?.ok_or("event not found")?.calendar_id;
    let (src, cred) = source_of_calendar(s, cal).await?;
    calendar::delete_event(&s.store, &src, cred.as_ref(), event_id).await.map_err(err)
}

// ---- offline copy, export, settings files --------------------------------------------

fn since_for(days: u32) -> Option<i64> {
    (days > 0).then(|| calendar::now() - days as i64 * 86400)
}

async fn offline_all(s: &AppState) -> Res<ops::OfflineReport> {
    let since = since_for(settings(s).offline_days);
    let accounts = lock(s)?.list_accounts().map_err(err)?;
    let mut total = ops::OfflineReport::default();
    for a in accounts {
        let (acct, cred) = credential(s, a.id).await?;
        let r = ops::download_offline(&s.store, &acct, &cred, since).await.map_err(err)?;
        total.listed += r.listed;
        total.downloaded += r.downloaded;
        total.freed += r.freed;
    }
    Ok(total)
}

/// "Download now" for the offline period in Settings.
#[tauri::command]
async fn download_offline_now(state: State<'_, AppState>) -> Res<ops::OfflineReport> {
    state.last_offline.store(calendar::now(), Ordering::SeqCst);
    offline_all(state.inner()).await
}

/// Where exported files go: Downloads, else Documents, else app data.
fn export_dir(app: &AppHandle) -> Res<std::path::PathBuf> {
    let p = app.path();
    p.download_dir().or_else(|_| p.document_dir()).or_else(|_| p.app_data_dir()).map_err(err)
}

fn stamp() -> String {
    let t = calendar::now();
    let (y, m, d) = mail_core::ical::civil_from_days(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02} {:02}{:02}", s / 3600, s % 3600 / 60)
}

/// Saves an attachment to Downloads after it passes the safety checks.
#[tauri::command]
async fn save_attachment(app: AppHandle, state: State<'_, AppState>, message_id: i64, index: usize) -> Res<String> {
    let s = state.inner();
    let path = ops::save_attachment(&s.store, message_id, index, &export_dir(&app)?, &scanner(s)).await.map_err(err)?;
    restrict_to_owner(&path);
    Ok(path.display().to_string())
}

/// Saves all mail of every account as .mbox files. Returns the folder.
#[tauri::command]
async fn export_mail(app: AppHandle, state: State<'_, AppState>) -> Res<(String, Vec<ops::ExportReport>)> {
    let s = state.inner();
    let dir = export_dir(&app)?.join(format!("Carteiro mail {}", stamp()));
    let mut reports = Vec::new();
    let accounts = lock(s)?.list_accounts().map_err(err)?;
    for a in accounts {
        let (acct, cred) = credential(s, a.id).await?;
        reports.push(ops::export_mbox(&s.store, &acct, &cred, &dir).await.map_err(err)?);
    }
    restrict_tree(&dir);
    Ok((dir.display().to_string(), reports))
}

#[tauri::command]
fn export_settings(app: AppHandle, state: State<'_, AppState>, ui: serde_json::Value) -> Res<String> {
    let s = state.inner();
    let prefs = serde_json::json!({ "settings": settings(s), "scanner": s.scanner.lock().map(|c| c.clone()).unwrap_or_default(), "ui": ui });
    let data = mail_core::settings_io::export(&*lock(s)?, prefs, calendar::now()).map_err(err)?;
    let path = export_dir(&app)?.join(format!("Carteiro settings {}.json", stamp()));
    std::fs::write(&path, serde_json::to_vec_pretty(&data).map_err(err)?).map_err(err)?;
    restrict_to_owner(&path);
    Ok(path.display().to_string())
}

#[derive(Serialize)]
struct ImportResult {
    report: mail_core::settings_io::ImportReport,
    ui: serde_json::Value,
}

#[tauri::command]
fn import_settings(state: State<'_, AppState>, json: String) -> Res<ImportResult> {
    let s = state.inner();
    let data = mail_core::settings_io::parse(&json).map_err(err)?;
    let report = mail_core::settings_io::import(&*lock(s)?, &data).map_err(err)?;
    if let Some(cfg) = data.app.get("settings").and_then(|v| serde_json::from_value::<AppSettings>(v.clone()).ok()) {
        lock(s)?.set_secret("app_settings", &serde_json::to_string(&cfg).map_err(err)?).map_err(err)?;
        *s.settings.lock().map_err(|_| "lock poisoned")? = cfg;
    }
    if let Some(cfg) = data.app.get("scanner").and_then(|v| serde_json::from_value::<ScannerConfig>(v.clone()).ok()) {
        lock(s)?.set_secret("scanner_config", &serde_json::to_string(&cfg).map_err(err)?).map_err(err)?;
        *s.scanner.lock().map_err(|_| "lock poisoned")? = cfg;
    }
    Ok(ImportResult { report, ui: data.app.get("ui").cloned().unwrap_or_default() })
}

#[tauri::command]
fn storage_usage(state: State<'_, AppState>) -> Res<(u64, u64)> {
    lock(&state)?.attachment_usage().map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(navigation_guard());
    #[cfg(not(target_os = "linux"))]
    let builder = builder.plugin(tauri_plugin_notification::init());
    builder
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            // Earlier names of the app: move their data over once.
            if !dir.exists() {
                let old = ["dev.vantage.mail", "dev.thunderbonx.mail"]
                    .iter()
                    .filter_map(|id| dir.parent().map(|p| p.join(id)))
                    .find(|p| p.exists());
                if let Some(old) = old {
                    let _ = std::fs::rename(&old, &dir);
                }
            }
            std::fs::create_dir_all(&dir)?;
            restrict_to_owner(&dir);
            // Debug builds on Android can't be given environment variables:
            // the local test server's CA may instead be placed in the data
            // folder (adb run-as). Release builds never trust it.
            #[cfg(all(debug_assertions, target_os = "android"))]
            if std::env::var_os("CARTEIRO_TEST_CA").is_none() && dir.join("test-ca.pem").exists() {
                std::env::set_var("CARTEIRO_TEST_CA", dir.join("test-ca.pem"));
            }
            let store = Store::open(&dir.join("mail.db"))?;
            for f in ["mail.db", "mail.db-wal", "mail.db-shm"] {
                restrict_to_owner(&dir.join(f));
            }
            store.ensure_local_calendar()?;
            let scanner = store
                .get_secret("scanner_config")
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            let app_settings: AppSettings = store
                .get_secret("app_settings")
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            // Private ClamAV engine; keeps its signatures up to date.
            let clam = Arc::new(clamav::Engine::new(&dir, app.path().resource_dir().ok().as_deref()));
            let engine = clam.clone();
            tauri::async_runtime::spawn(async move { engine.run().await });
            app.manage(AppState {
                store: Arc::new(Mutex::new(store)),
                scanner: Mutex::new(scanner),
                settings: Mutex::new(app_settings),
                syncing: AtomicBool::new(false),
                last_offline: std::sync::atomic::AtomicI64::new(0),
                tokens: Mutex::new(HashMap::new()),
                clam: clam.clone(),
                updater: updater::Updater::new(),
            });
            updater::spawn_background(app.handle().clone());
            // Tracker list: load the saved copy, then refresh it daily.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let h = handle.clone();
                let _ = tauri::async_runtime::spawn_blocking(move || load_trackers(&h)).await;
                loop {
                    let stale = calendar::now() - tracker_status_of(&handle).updated > 24 * 3600;
                    if stale {
                        let _ = update_trackers_inner(&handle).await;
                    }
                    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                }
            });
            if settings(&app.state::<AppState>()).notify {
                notify::ask_permission(app.handle());
            }
            #[cfg(desktop)]
            build_tray(app)?;

            // Tell the UI when the scanner becomes ready, so messages opened
            // while it was starting can be shown properly.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ready = false;
                loop {
                    let now = handle.state::<AppState>().clam.status().state == "ready";
                    if now && !ready {
                        let _ = handle.emit("scanner-ready", ());
                        let s = handle.state::<AppState>();
                        let _ = ops::index_clean_text(&s.store, &crate::scanner(&s)).await;
                    }
                    ready = now;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });

            // Background sync keeps running while the window is hidden.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let minutes = settings(&handle.state::<AppState>()).sync_minutes.max(1);
                    tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
                    sync_all(&handle).await;
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if cfg!(desktop) && settings(&window.state::<AppState>()).close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            guess_settings,
            add_account,
            remove_account,
            update_account,
            list_unified,
            test_scanner,
            get_settings,
            scanner_status,
            download_offline_now,
            export_mail,
            open_link,
            save_attachment,
            export_settings,
            import_settings,
            scanner_update_now,
            update_status,
            tracker_status,
            update_trackers,
            check_update,
            install_update,
            set_settings,
            oauth_sign_in,
            list_calendars,
            list_cal_sources,
            calendar_events,
            set_calendar_visible,
            sync_calendars,
            add_caldav_source,
            remove_cal_source,
            holiday_countries,
            holiday_regions,
            add_holiday_calendar,
            add_ics_calendar,
            add_local_calendar,
            remove_local_calendar,
            save_event,
            delete_event,
            sync_account,
            list_folders,
            list_threads,
            open_thread,
            set_flag,
            move_messages,
            delete_messages,
            send_message,
            list_rules,
            save_rule,
            delete_rule,
            reorder_rules,
            run_rules,
            get_scanner,
            set_scanner,
            storage_usage,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Carteiro")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                updater::on_exit(app);
            }
        });
}

