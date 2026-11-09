mod clamav;
mod notify;
mod backup;
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
use mail_core::contacts::{self, Contact};
use mail_core::store::{ContactImport, ContactSource, StoredContact};
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
    /// An offline download is running (only one at a time).
    offline_running: AtomicBool,
    /// Local time zone offset (minutes east of UTC), reported by the interface.
    utc_offset: std::sync::atomic::AtomicI32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Closing the window hides it to the tray; mail keeps syncing.
    close_to_tray: bool,
    /// Desktop notification for new Inbox mail.
    notify: bool,
    /// Background sync interval during working hours.
    sync_minutes: u64,
    /// Working hours: days (0 = Monday … 6 = Sunday) and minutes after
    /// local midnight. Outside them mail is checked every `off_hours_minutes`
    /// (0: only when you ask).
    work_days: Vec<u8>,
    work_start: u32,
    work_end: u32,
    off_hours_minutes: u64,
    /// Settings format: 1 once the 3-minute default became the working-hours
    /// schedule. Missing in older saved settings, hence 0 there (not Default's 1).
    #[serde(default)]
    schedule_version: u32,
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
            sync_minutes: 15,
            work_days: vec![0, 1, 2, 3, 4],
            work_start: 8 * 60,
            work_end: 18 * 60,
            off_hours_minutes: 60,
            schedule_version: 1,
            // Baked in at build time from CI secrets / env vars, if provided.
            google: build_client(option_env!("CARTEIRO_GOOGLE_CLIENT_ID"), option_env!("CARTEIRO_GOOGLE_CLIENT_SECRET")),
            microsoft: build_client(option_env!("CARTEIRO_MICROSOFT_CLIENT_ID"), None),
            offline_days: 30,
            auto_update: true,
        }
    }
}

/// Minutes between automatic checks at local time `local` (unix seconds
/// shifted to local time); 0 means no automatic check.
fn sync_interval(cfg: &AppSettings, local: i64) -> u64 {
    let day = local.div_euclid(86400);
    let weekday = (day + 3).rem_euclid(7) as u8; // 1970-01-01 was a Thursday
    let minute = (local.rem_euclid(86400) / 60) as u32;
    let working = cfg.work_days.contains(&weekday) && minute >= cfg.work_start && minute < cfg.work_end;
    if working { cfg.sync_minutes.max(5) } else { cfg.off_hours_minutes }
}

/// The interface's UTC offset in minutes (east positive), for working hours.
#[tauri::command]
fn set_utc_offset(state: State<'_, AppState>, minutes: i32) {
    state.utc_offset.store(minutes.clamp(-14 * 60, 14 * 60), Ordering::Relaxed);
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
fn list_threads(state: State<'_, AppState>, account_id: i64, folder: String, query: Option<String>, offset: u32, limit: Option<u32>) -> Res<Vec<ThreadSummary>> {
    lock(&state)?.list_threads(account_id, &folder, query.as_deref(), limit.unwrap_or(100).clamp(1, 5000), offset).map_err(err)
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
async fn open_thread(app: AppHandle, state: State<'_, AppState>, account_id: i64, thread_key: String) -> Res<Vec<MessageView>> {
    let s = state.inner();
    let (acct, pw) = credential(s, account_id).await?;
    let views = ops::open_thread(&s.store, &acct, &pw, &thread_key, &scanner(s)).await.map_err(err)?;
    calendar_changed(&app);
    Ok(views)
}

/// Tells the interface when mailed invitations changed the calendar.
fn calendar_changed(app: &AppHandle) {
    if ops::take_invites_changed() {
        let _ = app.emit("calendar-changed", ());
    }
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
    config.sync_minutes = config.sync_minutes.clamp(5, 240);
    config.off_hours_minutes = if config.off_hours_minutes == 0 { 0 } else { config.off_hours_minutes.clamp(15, 24 * 60) };
    config.work_days.retain(|d| *d < 7);
    config.work_start = config.work_start.min(24 * 60);
    config.work_end = config.work_end.min(24 * 60);
    // Sign-in app registrations come with the build; nothing to set here.
    let current = settings(&state);
    config.google = current.google;
    config.microsoft = current.microsoft;
    lock(&state)?.set_secret("app_settings", &serde_json::to_string(&config).map_err(err)?).map_err(err)?;
    *state.settings.lock().map_err(|_| "lock poisoned")? = config;
    Ok(())
}

/// Syncs every account, notifies about new Inbox mail and tells the UI to
/// refresh. Skips if a background sync is already running.
// ---- the core both the app and the background job use ----------------------------
//
// One database connection and one ClamAV engine per process: on Android the
// background job (MailJobService) runs in the app's process, and shares them
// with the app when it's open.

#[derive(Clone)]
struct Core {
    store: Arc<Mutex<Store>>,
    clam: Arc<clamav::Engine>,
}
static CORE: Mutex<Option<Core>> = Mutex::new(None);
/// The interface is running (its own loops sync, scan and notify).
static APP_UP: AtomicBool = AtomicBool::new(false);
/// A mail sync is running (app or background job): one at a time.
static SYNC_BUSY: AtomicBool = AtomicBool::new(false);

fn core(dir: &std::path::Path, resource_dir: Option<&std::path::Path>) -> Result<Core, String> {
    let mut slot = CORE.lock().map_err(|_| "core lock poisoned")?;
    if let Some(c) = slot.as_ref() {
        return Ok(c.clone());
    }
    // Debug builds on Android can't be given environment variables: the local
    // test server's CA may instead be placed in the data folder (adb
    // run-as). Release builds never trust it.
    #[cfg(all(debug_assertions, target_os = "android"))]
    if std::env::var_os("CARTEIRO_TEST_CA").is_none() && dir.join("test-ca.pem").exists() {
        std::env::set_var("CARTEIRO_TEST_CA", dir.join("test-ca.pem"));
    }
    let store = Store::open(&dir.join("mail.db")).map_err(err)?;
    for f in ["mail.db", "mail.db-wal", "mail.db-shm"] {
        restrict_to_owner(&dir.join(f));
    }
    store.ensure_local_calendar().map_err(err)?;
    // Settings saved by older versions are brought up to date once.
    let _ = load_settings(&store);
    // Mail indexed before attachment contents were searchable: once more.
    let _ = store.reindex_for_attachments();
    // Invitations to past meetings used to be skipped: look again, once.
    let _ = store.recheck_invites_once();
    // People from mail already here, once (new mail is collected as it arrives).
    if store.get_secret("contacts_collected").ok().flatten().is_none() {
        let _ = store.collect_from_stored_mail();
        let _ = store.set_secret("contacts_collected", "1");
    }
    let c = Core { store: Arc::new(Mutex::new(store)), clam: Arc::new(clamav::Engine::new(dir, resource_dir)) };
    *slot = Some(c.clone());
    Ok(c)
}

fn load_scanner(store: &Store) -> ScannerConfig {
    store.get_secret("scanner_config").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default()
}

fn load_settings(store: &Store) -> AppSettings {
    let mut app_settings: AppSettings = store
        .get_secret("app_settings")
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    // Settings saved by older versions: the old 3-minute default
    // becomes the working-hours schedule, once.
    if app_settings.schedule_version < 1 {
        let fresh = AppSettings::default();
        if app_settings.sync_minutes <= 3 {
            app_settings.sync_minutes = fresh.sync_minutes;
        }
        app_settings.work_days = fresh.work_days;
        app_settings.work_start = fresh.work_start;
        app_settings.work_end = fresh.work_end;
        app_settings.off_hours_minutes = fresh.off_hours_minutes;
        app_settings.schedule_version = 1;
        let _ = store.set_secret("app_settings", &serde_json::to_string(&app_settings).unwrap_or_default());
    }
    // Sign-in registrations come with the build when none were saved.
    let built = AppSettings::default();
    if app_settings.google.client_id.is_empty() {
        app_settings.google = built.google;
    }
    if app_settings.microsoft.client_id.is_empty() {
        app_settings.microsoft = built.microsoft;
    }
    app_settings
}

/// Background work on Android (MailJobService), with the app closed or not:
/// checks every account for new mail, and with `deep` (charging, unmetered,
/// idle: at night) also updates the virus signatures when a week old,
/// downloads the offline period and checks it with ClamAV, so opening mail
/// later is quick. Returns notifications to show: (title, text).
pub fn background_run(dir: &std::path::Path, deep: bool) -> BackgroundReport {
    tauri::async_runtime::block_on(async {
        if SYNC_BUSY.swap(true, Ordering::SeqCst) {
            return BackgroundReport { log: vec!["skipped: a sync is already running".into()], ..Default::default() };
        }
        let mut report = BackgroundReport::default();
        if let Err(e) = background_inner(dir, deep, &mut report).await {
            report.log.push(e);
        }
        SYNC_BUSY.store(false, Ordering::SeqCst);
        report
    })
}

/// What a background run did: notifications to show, and a log for logcat.
#[derive(Serialize, Default)]
pub struct BackgroundReport {
    notify: Vec<(String, String)>,
    log: Vec<String>,
}

async fn background_inner(dir: &std::path::Path, deep: bool, report: &mut BackgroundReport) -> Res<()> {
    let core = core(dir, None)?;
    let (scanner_cfg, cfg) = {
        let st = core.store.lock().map_err(|_| "database lock poisoned")?;
        (load_scanner(&st), load_settings(&st))
    };
    let s = AppState {
        store: core.store.clone(),
        scanner: Mutex::new(scanner_cfg),
        settings: Mutex::new(cfg.clone()),
        syncing: AtomicBool::new(false),
        last_offline: std::sync::atomic::AtomicI64::new(0),
        tokens: Mutex::new(HashMap::new()),
        clam: core.clam.clone(),
        updater: updater::Updater::new(),
        utc_offset: std::sync::atomic::AtomicI32::new(0),
        offline_running: AtomicBool::new(false),
    };
    let accounts = lock(&s)?.list_accounts().map_err(err)?;
    let mut arrived = Vec::new();
    for a in &accounts {
        let (acct, pw) = match credential(&s, a.id).await {
            Ok(c) => c,
            Err(e) => {
                report.log.push(format!("{}: {e}", a.email));
                continue;
            }
        };
        match sync::sync_account(&s.store, &acct, &pw).await {
            Ok(r) => {
                report.log.push(format!("{}: {} new, {} arrived in the Inbox", a.email, r.new_messages, r.arrived.len()));
                arrived.extend(r.arrived);
            }
            Err(e) => report.log.push(format!("{}: {e}", a.email)),
        }
        if deep {
            match ops::download_offline(&s.store, &acct, &pw, since_for(cfg.offline_days)).await {
                Ok(r) => report.log.push(format!("{}: {} downloaded for offline use", a.email, r.downloaded)),
                Err(e) => report.log.push(format!("{}: offline download: {e}", a.email)),
            }
        }
    }
    if deep {
        // Weekly signatures, then check and index what was downloaded.
        let started_here = s.clam.addr().is_none();
        if s.clam.update_due() {
            let _ = s.clam.update().await;
        }
        match s.clam.ensure_running().await {
            Ok(()) => match ops::index_clean_text(&s.store, &scanner(&s)).await {
                Ok(n) => report.log.push(format!("{n} messages checked with ClamAV and indexed")),
                Err(e) => report.log.push(format!("indexing: {e}")),
            },
            Err(e) => report.log.push(format!("ClamAV: {e}")),
        }
        // Without the app, ClamAV's memory is given back until next time.
        if started_here && !APP_UP.load(Ordering::SeqCst) {
            s.clam.stop().await;
        }
    }
    if !cfg.notify || arrived.is_empty() {
        return Ok(());
    }
    report.notify = match arrived.as_slice() {
        [one] => vec![(one.from.clone(), if one.subject.is_empty() { "(no subject)".into() } else { one.subject.clone() })],
        many => vec![(format!("{} new messages", many.len()), many.iter().take(3).map(|m| m.from.as_str()).collect::<Vec<_>>().join(", "))],
    };
    Ok(())
}

/// Android's MailJobService calls this (see src-tauri/android/MailJobService.kt).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_dev_carteiro_mail_MailJobService_runInBackground<'a>(
    mut env: jni::JNIEnv<'a>,
    _this: jni::objects::JObject<'a>,
    data_dir: jni::objects::JString<'a>,
    deep: jni::sys::jboolean,
) -> jni::sys::jstring {
    let dir: String = env.get_string(&data_dir).map(Into::into).unwrap_or_default();
    let report = if dir.is_empty() { BackgroundReport::default() } else { background_run(std::path::Path::new(&dir), deep != 0) };
    let json = serde_json::to_string(&report).unwrap_or_else(|_| "{}".into());
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Tells the interface what's running in the background (`label`), or that
/// it ended (None), so it can show it.
fn activity(app: &AppHandle, key: &str, label: Option<String>) {
    let _ = app.emit("activity", serde_json::json!({ "key": key, "label": label }));
}

async fn sync_all(app: &AppHandle) {
    let s = app.state::<AppState>();
    if SYNC_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    s.syncing.store(true, Ordering::SeqCst);
    let accounts = lock(&s).and_then(|st| st.list_accounts().map_err(err)).unwrap_or_default();
    let mut arrived = Vec::new();
    for a in &accounts {
        let Ok((_, pw)) = credential(&s, a.id).await else { continue };
        activity(app, "sync", Some(format!("Checking mail ({})", a.email)));
        if let Ok(r) = sync::sync_account(&s.store, a, &pw).await {
            arrived.extend(r.arrived);
        }
    }
    activity(app, "sync", Some("Updating calendars".into()));
    let _ = sync_calendars_inner(&s).await;
    activity(app, "sync", Some("Updating contacts".into()));
    let _ = sync_contacts_inner(&s).await;
    activity(app, "sync", None);
    // Keep the chosen period available offline, at most hourly.
    if calendar::now() - s.last_offline.load(Ordering::SeqCst) > 3600 {
        s.last_offline.store(calendar::now(), Ordering::SeqCst);
        let _ = offline_all(app, &s).await;
    }
    // Content search covers only mail the virus scanner found clean.
    activity(app, "index", Some("Checking new mail with ClamAV and indexing it for search".into()));
    let _ = ops::index_clean_text(&s.store, &scanner(&s)).await;
    activity(app, "index", None);
    calendar_changed(app);
    s.syncing.store(false, Ordering::SeqCst);
    SYNC_BUSY.store(false, Ordering::SeqCst);
    if !arrived.is_empty() && settings(&s).notify {
        let (title, body) = match arrived.as_slice() {
            [one] => (one.from.clone(), if one.subject.is_empty() { "(no subject)".to_string() } else { one.subject.clone() }),
            many => (format!("{} new messages", many.len()), many.iter().take(3).map(|m| m.from.as_str()).collect::<Vec<_>>().join(", ")),
        };
        notify::show(app, &title, &body);
    }
    let _ = app.emit("mail-synced", arrived.len());
}

#[cfg(desktop)]
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
    ensure_account_contacts(&st, &account).map_err(err)?;
    Ok(SignInResult::Account { account })
}

// ---- contacts --------------------------------------------------------------------------

/// Google / Microsoft accounts come with their address book (accounts
/// added before contacts existed get it on the next sync).
fn ensure_account_contacts(st: &Store, a: &Account) -> mail_core::Result<()> {
    let kind = match a.auth {
        AuthMethod::Google => "google",
        AuthMethod::Microsoft => "microsoft",
        AuthMethod::Password => return Ok(()),
    };
    if st.contact_sources()?.iter().any(|c| c.account_id == Some(a.id)) {
        return Ok(());
    }
    let url = if kind == "google" { contacts::google_carddav_url(&a.email) } else { String::new() };
    st.add_contact_source(kind, Some(a.id), &url, &a.email, &a.email)?;
    Ok(())
}

async fn contact_credential(s: &AppState, src: &ContactSource) -> Res<Credential> {
    match src.kind.as_str() {
        "carddav" => Ok(Credential::Password(secrets::get(&*lock(s)?, &format!("contacts-{}", src.id))?)),
        kind => {
            let provider = if kind == "google" { AuthMethod::Google } else { AuthMethod::Microsoft };
            let account = src.account_id.ok_or("this address book has no account")?;
            oauth_token(s, provider, &format!("oauth-acct-{account}"), Purpose::Contacts).await
        }
    }
}

async fn sync_contacts_inner(s: &AppState) -> Vec<String> {
    if let Ok(st) = lock(s) {
        for a in st.list_accounts().unwrap_or_default() {
            let _ = ensure_account_contacts(&st, &a);
        }
    }
    let sources = lock(s).and_then(|st| st.contact_sources().map_err(err)).unwrap_or_default();
    let mut errors = Vec::new();
    // Your own, collected and phone contacts have no server to sync with.
    for src in sources.iter().filter(|x| !matches!(x.kind.as_str(), "local" | "collected" | "android")) {
        let r = match contact_credential(s, src).await {
            Ok(cred) => contacts::sync_source(&s.store, src, &cred).await.map_err(err),
            Err(e) => Err(e),
        };
        if let Err(e) = r {
            errors.push(format!("{}: {e}", src.name));
        }
    }
    errors
}

/// The phone's address book, read by the Android activity and mirrored
/// here as "Phone contacts" (read-only; change them in the phone's app).
#[tauri::command]
fn sync_phone_contacts(state: State<'_, AppState>, contacts: Vec<(String, Contact)>) -> Res<usize> {
    let mut st = lock(&state)?;
    let source = match st.contact_sources().map_err(err)?.into_iter().find(|s| s.kind == "android") {
        Some(s) => s.id,
        None => st.add_contact_source("android", None, "", "", "Phone contacts").map_err(err)?,
    };
    let list: Vec<_> = contacts.into_iter().filter_map(|(id, c)| c.normalized().map(|c| (id, None, c))).collect();
    st.replace_contacts(source, &list).map_err(err)?;
    Ok(list.len())
}

/// Your own answer to an event: "yes", "maybe", "no" or "". Only noted on
/// this device; the organizer and other guests are never told.
/// Sends your answer for one event to its organizer, from the account the
/// invitation came to. Only when you press "Send answer".
#[tauri::command]
async fn send_rsvp(state: State<'_, AppState>, event_id: i64) -> Res<ops::SendResult> {
    let s = state.inner();
    let account_id = ops::invite_account(&s.store, event_id).map_err(err)?.or_else(|| lock(s).ok()?.list_accounts().ok()?.first().map(|a| a.id));
    let account_id = account_id.ok_or("Add an account to send answers")?;
    let (account, cred) = credential(s, account_id).await?;
    ops::send_rsvp(&s.store, &account, &cred, event_id).await.map_err(err)
}

#[derive(Serialize)]
struct EventSource {
    message_id: i64,
    account_id: i64,
    thread_key: String,
    folder: String,
    subject: String,
    from: String,
    date: i64,
    /// What the message did to the meeting: "added", "cancelled", "answer"…
    outcome: String,
}

/// The mail a meeting from an invitation came from (oldest first).
#[tauri::command]
fn event_sources(state: State<'_, AppState>, event_id: i64) -> Res<Vec<EventSource>> {
    let st = lock(&state)?;
    let Some(ev) = st.event(event_id).map_err(err)? else { return Ok(Vec::new()) };
    if ev.remote_id.is_empty() {
        return Ok(Vec::new());
    }
    // A copy kept apart ("uid#sender") came from the same mail as its uid.
    let mut out = Vec::new();
    for (id, outcome) in st.invite_sources(&ev.remote_id).map_err(err)? {
        if let Some(m) = st.message(id).map_err(err)? {
            let from = if m.from.name.is_empty() { m.from.email.clone() } else { m.from.name.clone() };
            out.push(EventSource { message_id: id, account_id: m.account_id, thread_key: m.thread_key, folder: m.folder, subject: m.subject, from, date: m.date, outcome });
        }
    }
    Ok(out)
}

/// For diagnosis: the shapes (no contents) of calendar parts in up to
/// `limit` messages whose calendar file wasn't readable.
#[tauri::command]
fn invite_diagnostics(state: State<'_, AppState>, limit: u32) -> Res<Vec<serde_json::Value>> {
    let st = lock(&state)?;
    let ids = st.unreadable_invite_msgs(limit).map_err(err)?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(raw) = st.raw(id).map_err(err)? {
            out.push(serde_json::json!(mail_core::invites::calendar_part_shapes(&raw)));
        }
    }
    Ok(out)
}

/// How messages carrying calendar files fared (counts per outcome).
#[tauri::command]
fn invite_stats(state: State<'_, AppState>) -> Res<Vec<(String, i64)>> {
    lock(&state)?.invite_stats().map_err(err)
}

/// Removes dates of a repeating event laid out on this device (invitations):
/// those starting in [from, to), or all of them when both are absent.
#[tauri::command]
fn remove_series(state: State<'_, AppState>, event_id: i64, from: Option<i64>, to: Option<i64>) -> Res<usize> {
    lock(&state)?.remove_series_range(event_id, from, to).map_err(err)
}

#[tauri::command]
fn set_rsvp(state: State<'_, AppState>, event_id: i64, rsvp: String) -> Res<()> {
    lock(&state)?.set_rsvp(event_id, &rsvp).map_err(err)
}

#[tauri::command]
async fn sync_contacts(state: State<'_, AppState>) -> Res<Vec<String>> {
    Ok(sync_contacts_inner(state.inner()).await)
}

#[tauri::command]
fn list_contacts(state: State<'_, AppState>, query: String) -> Res<Vec<StoredContact>> {
    lock(&state)?.list_contacts(&query, 5000).map_err(err)
}

#[tauri::command]
fn save_contact(state: State<'_, AppState>, id: Option<i64>, contact: Contact) -> Res<i64> {
    let c = contact.normalized().ok_or("A contact needs a name, an email address or a phone number")?;
    lock(&state)?.save_contact(id, &c).map_err(err)
}

#[tauri::command]
fn delete_contact(state: State<'_, AppState>, id: i64) -> Res<()> {
    lock(&state)?.delete_contact(id).map_err(err)
}

/// Imports a vCard, CSV (Google / Outlook / Thunderbird) or LDIF file.
#[tauri::command]
fn import_contacts(state: State<'_, AppState>, text: String) -> Res<ContactImport> {
    let list = contacts::parse_any(&text);
    if list.is_empty() {
        return Err("No contacts found in this file (vCard, CSV or LDIF expected)".into());
    }
    lock(&state)?.import_contacts(list).map_err(err)
}

/// All contacts as one vCard file in Downloads/Carteiro.
#[tauri::command]
fn export_contacts(app: AppHandle, state: State<'_, AppState>, stamp_local: Option<String>) -> Res<String> {
    let all = lock(&state)?.list_contacts("", u32::MAX).map_err(err)?;
    let text: String = all.iter().map(|c| contacts::to_vcard(&c.contact)).collect();
    let path = export_dir(&app)?.join(format!("Carteiro contacts {}.vcf", stamp(stamp_local.as_deref())));
    std::fs::write(&path, text).map_err(err)?;
    restrict_to_owner(&path);
    Ok(path.display().to_string())
}

#[tauri::command]
fn list_contact_sources(state: State<'_, AppState>) -> Res<Vec<ContactSource>> {
    let st = lock(&state)?;
    st.ensure_local_contacts().map_err(err)?;
    st.contact_sources().map_err(err)
}

/// A CardDAV address book (Fastmail, iCloud, Nextcloud...): checked, saved, synced.
#[tauri::command]
async fn add_carddav_contacts(state: State<'_, AppState>, url: String, username: String, password: String, name: String) -> Res<i64> {
    let s = state.inner();
    let dav = mail_core::caldav::CalDav { user: username.clone(), cred: Credential::Password(password.clone()) };
    mail_core::carddav::discover(&dav, &url).await.map_err(err)?;
    let id = {
        let st = lock(s)?;
        let id = st.add_contact_source("carddav", None, &url, &username, if name.is_empty() { &url } else { &name }).map_err(err)?;
        secrets::set(&st, &format!("contacts-{id}"), &password)?;
        id
    };
    let _ = sync_contacts_inner(s).await;
    Ok(id)
}

#[tauri::command]
fn remove_contact_source(state: State<'_, AppState>, id: i64) -> Res<()> {
    lock(&state)?.remove_contact_source(id).map_err(err)
}

#[tauri::command]
fn suggest_addresses(state: State<'_, AppState>, prefix: String) -> Res<Vec<(String, String)>> {
    lock(&state)?.suggest_addresses(&prefix, 8).map_err(err)
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
fn search_events(state: State<'_, AppState>, query: String) -> Res<Vec<StoredEvent>> {
    lock(&state)?.search_events(&query, 200).map_err(err)
}

#[tauri::command]
fn set_calendar_visible(state: State<'_, AppState>, calendar_id: i64, visible: bool) -> Res<()> {
    lock(&state)?.set_calendar_visible(calendar_id, visible).map_err(err)
}

async fn sync_calendars_inner(s: &AppState) -> Vec<String> {
    let sources = lock(s).and_then(|st| st.cal_sources().map_err(err)).unwrap_or_default();
    let mut errors = Vec::new();
    // Your own, collected and phone contacts have no server to sync with.
    for src in sources.iter().filter(|x| !matches!(x.kind.as_str(), "local" | "collected" | "android")) {
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

/// Downloads the offline period for every account, telling the interface
/// how far it is ("offline-progress", then "offline-done"). One at a time.
async fn offline_all(app: &AppHandle, s: &AppState) -> Res<ops::OfflineReport> {
    if s.offline_running.swap(true, Ordering::SeqCst) {
        return Err("Mail is already being downloaded for offline use".into());
    }
    let result = async {
        let since = since_for(settings(s).offline_days);
        let accounts = lock(s)?.list_accounts().map_err(err)?;
        let mut total = ops::OfflineReport::default();
        let last = std::sync::atomic::AtomicI64::new(0);
        let progress = |p: ops::OfflineProgress| {
            // At most a few updates a second.
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
            if now - last.load(Ordering::Relaxed) >= 250 || p.done == 0 {
                last.store(now, Ordering::Relaxed);
                let _ = app.emit("offline-progress", &p);
            }
        };
        for a in accounts {
            let (acct, cred) = credential(s, a.id).await?;
            let r = ops::download_offline_with(&s.store, &acct, &cred, since, &progress).await.map_err(err)?;
            total.listed += r.listed;
            total.downloaded += r.downloaded;
            total.freed += r.freed;
        }
        Ok(total)
    }
    .await;
    s.offline_running.store(false, Ordering::SeqCst);
    let done = match &result {
        Ok(r) => serde_json::json!({ "report": r }),
        Err(e) => serde_json::json!({ "error": e }),
    };
    let _ = app.emit("offline-done", done);
    result
}

/// "Download now" for the offline period in Settings: starts in the
/// background (progress arrives as events) and returns at once.
#[tauri::command]
fn download_offline_now(app: AppHandle, state: State<'_, AppState>) -> Res<()> {
    if state.offline_running.load(Ordering::SeqCst) {
        return Err("Mail is already being downloaded for offline use".into());
    }
    state.last_offline.store(calendar::now(), Ordering::SeqCst);
    tauri::async_runtime::spawn(async move {
        let s = app.state::<AppState>();
        let _ = offline_all(&app, &s).await;
    });
    Ok(())
}

/// The messages an action on a whole conversation touches: every copy in
/// `folder`, or in all folders when it's None.
#[tauri::command]
fn thread_ids(state: State<'_, AppState>, account_id: i64, thread_key: String, folder: Option<String>) -> Res<Vec<i64>> {
    lock(&state)?.thread_message_ids(account_id, &thread_key, folder.as_deref()).map_err(err)
}

#[derive(Serialize)]
struct SearchHits {
    /// Message-IDs of the matching messages, newest first.
    message_ids: Vec<String>,
    /// Attachment extensions the search asked for (has:pdf…), to highlight.
    extensions: Vec<String>,
}

/// Where a search matched inside one conversation.
#[tauri::command]
fn search_hits(state: State<'_, AppState>, account_id: i64, folder: String, thread_key: String, query: String) -> Res<SearchHits> {
    let st = lock(&state)?;
    let ids = st.search_hits(account_id, &folder, &thread_key, &query).map_err(err)?;
    // The originals, not replies that only quote them or re-send their files.
    let ids = ops::own_hits(&st, account_id, &thread_key, &ids, &query).map_err(err)?;
    let mut message_ids = Vec::new();
    for id in ids {
        if let Some(m) = st.message(id).map_err(err)? {
            message_ids.push(m.message_id);
        }
    }
    let extensions = mail_core::search::parse(&query).attachment_types.concat();
    Ok(SearchHits { message_ids, extensions })
}

/// Lists older messages of a folder (the first sync lists the latest only).
#[tauri::command]
async fn load_older(state: State<'_, AppState>, account_id: i64, folder: String) -> Res<usize> {
    let s = state.inner();
    let (acct, cred) = credential(s, account_id).await?;
    ops::load_older(&s.store, &acct, &cred, &folder, 200).await.map_err(err)
}

/// Where exported files go: Downloads, else Documents, else app data.
/// Where saved attachments and exports go: Downloads/Carteiro.
fn export_dir(app: &AppHandle) -> Res<std::path::PathBuf> {
    // Android 11+ lets apps create their own files in the shared Download
    // folder without a permission; older versions fall back to the app's folder.
    #[cfg(target_os = "android")]
    {
        let public = std::path::PathBuf::from("/storage/emulated/0/Download/Carteiro");
        let probe = public.join(".carteiro-write-test");
        if std::fs::create_dir_all(&public).is_ok() && std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return Ok(public);
        }
    }
    let p = app.path();
    let base = p.download_dir().or_else(|_| p.document_dir()).or_else(|_| p.app_data_dir()).map_err(err)?;
    let dir = base.join("Carteiro");
    std::fs::create_dir_all(&dir).map_err(err)?;
    Ok(dir)
}

/// The local time the UI sent for file names ("2026-11-08 1407"); the engine
/// has no time zone database. Anything unexpected falls back to UTC.
fn stamp(local: Option<&str>) -> String {
    if let Some(l) = local.filter(|l| l.len() <= 20 && l.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b' ')) {
        return l.to_string();
    }
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

/// What a calendar attachment holds (after the same checks as saving it).
#[tauri::command]
async fn preview_ics(state: State<'_, AppState>, message_id: i64, index: usize) -> Res<ops::IcsPreview> {
    let s = state.inner();
    ops::preview_ics(&s.store, message_id, index, &scanner(s)).await.map_err(err)
}

/// Adds a calendar attachment to the Invitations calendar, at your request.
#[tauri::command]
async fn add_ics(state: State<'_, AppState>, message_id: i64, index: usize) -> Res<usize> {
    let s = state.inner();
    ops::add_ics(&s.store, message_id, index, &scanner(s)).await.map_err(err)
}

/// Saves all mail of every account as .mbox files. Returns the folder.
#[tauri::command]
async fn export_mail(app: AppHandle, state: State<'_, AppState>, stamp_local: Option<String>) -> Res<(String, Vec<ops::ExportReport>)> {
    let s = state.inner();
    let dir = export_dir(&app)?.join(format!("Carteiro mail {}", stamp(stamp_local.as_deref())));
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
fn export_settings(app: AppHandle, state: State<'_, AppState>, ui: serde_json::Value, stamp_local: Option<String>) -> Res<String> {
    let s = state.inner();
    let prefs = serde_json::json!({ "settings": settings(s), "scanner": s.scanner.lock().map(|c| c.clone()).unwrap_or_default(), "ui": ui });
    let data = mail_core::settings_io::export(&*lock(s)?, prefs, calendar::now()).map_err(err)?;
    let path = export_dir(&app)?.join(format!("Carteiro settings {}.json", stamp(stamp_local.as_deref())));
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

// ---- full backup (see backup.rs) ----------------------------------------------

/// Writes the backup into `folder` (Downloads/Carteiro when empty).
#[tauri::command]
async fn create_backup(app: AppHandle, state: State<'_, AppState>, folder: Option<String>, passphrase: String, ui: serde_json::Value, stamp_local: Option<String>) -> Res<String> {
    let dest_dir = match folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => std::path::PathBuf::from(f),
        None => export_dir(&app)?,
    };
    let data = app.path().app_data_dir().map_err(err)?;
    let store = state.store.clone();
    let stamp = stamp(stamp_local.as_deref());
    let dest = tauri::async_runtime::spawn_blocking(move || backup::create(&data, &store, &dest_dir, &passphrase, &ui, &stamp)).await.map_err(err)??;
    Ok(dest.display().to_string())
}

/// The UI sends the chosen backup file in base64 pieces (it may be large,
/// the web view can't hand over a path, and Android's bridge carries text
/// only). `first` starts a new upload.
#[tauri::command]
fn restore_upload(app: AppHandle, data: String, first: bool) -> Res<()> {
    use std::io::Write;
    let bytes = mail_core::b64::decode(&data).ok_or("damaged upload")?;
    let path = app.path().app_data_dir().map_err(err)?.join(backup::UPLOAD);
    let mut f = std::fs::OpenOptions::new().create(true).append(!first).write(true).truncate(first).open(&path).map_err(err)?;
    restrict_to_owner(&path);
    f.write_all(&bytes).map_err(err)
}

#[derive(Serialize)]
struct Restored {
    accounts: usize,
    ui: serde_json::Value,
}

/// Replaces everything on this install with the uploaded backup.
#[tauri::command]
async fn restore_backup(app: AppHandle, state: State<'_, AppState>, passphrase: String) -> Res<Restored> {
    let data = app.path().app_data_dir().map_err(err)?;
    let store = state.store.clone();
    let r = tauri::async_runtime::spawn_blocking(move || backup::restore(&data, &store, &passphrase)).await.map_err(err)??;
    let s = state.inner();
    let st = lock(s)?;
    if let Some(cfg) = st.get_secret("app_settings").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()) {
        *s.settings.lock().map_err(|_| "lock poisoned")? = cfg;
    }
    if let Some(cfg) = st.get_secret("scanner_config").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()) {
        *s.scanner.lock().map_err(|_| "lock poisoned")? = cfg;
    }
    s.tokens.lock().map_err(|_| "lock poisoned")?.clear();
    Ok(Restored { accounts: r.accounts, ui: r.ui })
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
            let core = core(&dir, app.path().resource_dir().ok().as_deref())?;
            APP_UP.store(true, Ordering::SeqCst);
            let (scanner, app_settings) = {
                let st = core.store.lock().map_err(|_| "database lock poisoned")?;
                (load_scanner(&st), load_settings(&st))
            };
            let clam = core.clam.clone();
            let engine = clam.clone();
            tauri::async_runtime::spawn(async move { engine.run().await });
            app.manage(AppState {
                store: core.store.clone(),
                scanner: Mutex::new(scanner),
                settings: Mutex::new(app_settings),
                syncing: AtomicBool::new(false),
                last_offline: std::sync::atomic::AtomicI64::new(0),
                tokens: Mutex::new(HashMap::new()),
                clam: clam.clone(),
                updater: updater::Updater::new(),
                utc_offset: std::sync::atomic::AtomicI32::new(0),
                offline_running: AtomicBool::new(false),
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
                        activity(&handle, "index", Some("Checking mail with ClamAV and indexing it for search".into()));
                        let _ = ops::index_clean_text(&s.store, &crate::scanner(&s)).await;
                        activity(&handle, "index", None);
                        calendar_changed(&handle);
                    }
                    ready = now;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });

            // Background sync keeps running while the window is hidden:
            // often during working hours, rarely (or never) outside them.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut last = calendar::now();
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let s = handle.state::<AppState>();
                    let now = calendar::now();
                    let every = sync_interval(&settings(&s), now + i64::from(s.utc_offset.load(Ordering::Relaxed)) * 60);
                    if every > 0 && now - last >= every as i64 * 60 {
                        last = now;
                        sync_all(&handle).await;
                    }
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
            load_older,
            thread_ids,
            search_hits,
            export_mail,
            open_link,
            save_attachment,
            export_settings,
            import_settings,
            create_backup,
            sync_phone_contacts,
            set_rsvp,
            preview_ics,
            add_ics,
            remove_series,
            invite_stats,
            event_sources,
            invite_diagnostics,
            send_rsvp,
            set_utc_offset,
            restore_upload,
            restore_backup,
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
            list_contacts,
            save_contact,
            delete_contact,
            import_contacts,
            export_contacts,
            list_contact_sources,
            add_carddav_contacts,
            remove_contact_source,
            sync_contacts,
            suggest_addresses,
            search_events,
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

#[cfg(test)]
mod schedule_tests {
    use super::*;

    #[test]
    fn checks_often_in_working_hours_rarely_outside() {
        let cfg = AppSettings::default();
        // 2026-11-09 was a Monday.
        let at = |day: i64, h: i64, m: i64| mail_core::ical::days_from_civil(2026, 11, 9 + day as u32) * 86400 + h * 3600 + m * 60;
        assert_eq!(sync_interval(&cfg, at(0, 9, 0)), 15, "Monday morning");
        assert_eq!(sync_interval(&cfg, at(4, 17, 59)), 15, "Friday, just before 18:00");
        assert_eq!(sync_interval(&cfg, at(4, 18, 0)), 60, "Friday evening");
        assert_eq!(sync_interval(&cfg, at(0, 7, 59)), 60, "before work");
        assert_eq!(sync_interval(&cfg, at(5, 11, 0)), 60, "Saturday");
        let never = AppSettings { off_hours_minutes: 0, ..AppSettings::default() };
        assert_eq!(sync_interval(&never, at(6, 11, 0)), 0, "Sunday: only when asked");
        // Settings saved before working hours existed (old 3-minute default).
        let old: AppSettings = serde_json::from_str(r#"{"sync_minutes":3}"#).unwrap();
        assert_eq!(old.schedule_version, 0, "so the one-time upgrade runs");
    }
}
