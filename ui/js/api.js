// Backend calls. Inside Tauri they go to the Rust commands; in a plain
// browser (opening ui/ via any static server) a mock backend is used.

const tauri = window.__TAURI__?.core;

async function call(cmd, args = {}) {
  if (tauri) return tauri.invoke(cmd, args);
  const { mock } = await import("./mock.js");
  return mock(cmd, args);
}

// Query parameters that only identify the recipient or the campaign.
const TRACKING_PARAMS = /^(utm_\w+|fbclid|gclid|dclid|gbraid|wbraid|msclkid|yclid|mc_cid|mc_eid|_hsenc|_hsmi|mkt_tok|igshid|oly_anon_id|oly_enc_id|vero_id|vero_conv|rb_clickid|s_cid|ml_subscriber|ml_subscriber_hash|trk|trkcampaign|sc_campaign|sc_channel|sc_content|sc_medium|sc_outcome|sc_geo|sc_country|wt_mc|ck_subscriber_id|__s)$/i;

/// Removes tracking parameters from web links (mailto: and others unchanged).
export function withoutTracking(link) {
  try {
    const u = new URL(link);
    if (!/^https?:$/.test(u.protocol)) return link;
    for (const k of [...u.searchParams.keys()]) if (TRACKING_PARAMS.test(k)) u.searchParams.delete(k);
    return u.toString();
  } catch {
    return link;
  }
}

export const api = {
  listAccounts: () => call("list_accounts"),
  guessSettings: (email) => call("guess_settings", { email }),
  addAccount: (account, password, verify = true) => call("add_account", { account, password, verify }),
  removeAccount: (accountId) => call("remove_account", { accountId }),
  updateAccount: (account, password = null) => call("update_account", { account, password }),
  unified: (query) => call("list_unified", { query }),
  testScanner: (addr) => call("test_scanner", { addr }),
  getSettings: () => call("get_settings"),
  setSettings: (config) => call("set_settings", { config }),
  sync: (accountId) => call("sync_account", { accountId }),
  folders: (accountId) => call("list_folders", { accountId }),
  threads: (accountId, folder, query, offset = 0) => call("list_threads", { accountId, folder, query, offset }),
  openThread: (accountId, threadKey) => call("open_thread", { accountId, threadKey }),
  setFlag: (accountId, ids, flag, on) => call("set_flag", { accountId, ids, flag, on }),
  move: (accountId, ids, folder) => call("move_messages", { accountId, ids, folder }),
  remove: (accountId, ids) => call("delete_messages", { accountId, ids }),
  send: (accountId, draft) => call("send_message", { accountId, draft }),
  rules: () => call("list_rules"),
  saveRule: (rule) => call("save_rule", { rule }),
  deleteRule: (id) => call("delete_rule", { id }),
  reorderRules: (ids) => call("reorder_rules", { ids }),
  runRules: (accountId, folder) => call("run_rules", { accountId, folder }),
  getScanner: () => call("get_scanner"),
  scannerStatus: () => call("scanner_status"),
  downloadOfflineNow: () => call("download_offline_now"),
  exportMail: () => call("export_mail"),
  saveAttachment: (messageId, index) => call("save_attachment", { messageId, index }),
  exportSettings: (ui) => call("export_settings", { ui }),
  importSettings: (json) => call("import_settings", { json }),
  scannerUpdateNow: () => call("scanner_update_now"),
  setScanner: (config) => call("set_scanner", { config }),
  storageUsage: () => call("storage_usage"),
  updateStatus: () => call("update_status"),
  trackerStatus: () => call("tracker_status"),
  updateTrackers: () => call("update_trackers"),
  checkUpdate: () => call("check_update"),
  installUpdate: () => call("install_update"),
  // Tracking parameters are dropped first. Android opens links through the
  // activity (src-tauri/android/MainActivity.kt).
  openLink: (link) => { const url = withoutTracking(link); return window.CarteiroAndroid
    ? (window.CarteiroAndroid.open(url) ? Promise.resolve() : Promise.reject("Only web and email links can be opened"))
    : window.__TAURI__ ? call("open_link", { url }) : Promise.resolve(window.open(url, "_blank", "noopener")); },
  oauthSignIn: (provider, what) => call("oauth_sign_in", { provider, what }),
  calendars: () => call("list_calendars"),
  calSources: () => call("list_cal_sources"),
  events: (from, to) => call("calendar_events", { from, to }),
  searchEvents: (query) => call("search_events", { query }),
  setCalendarVisible: (calendarId, visible) => call("set_calendar_visible", { calendarId, visible }),
  syncCalendars: () => call("sync_calendars"),
  addCaldav: (url, username, password, name) => call("add_caldav_source", { url, username, password, name }),
  removeCalSource: (sourceId) => call("remove_cal_source", { sourceId }),
  saveEvent: (calendarId, eventId, event) => call("save_event", { calendarId, eventId, event }),
  deleteEvent: (eventId) => call("delete_event", { eventId }),
  holidayCountries: () => call("holiday_countries"),
  holidayRegions: (country) => call("holiday_regions", { country }),
  addHolidayCalendar: (country, region, name) => call("add_holiday_calendar", { country, region, name }),
  addIcsCalendar: (url, name) => call("add_ics_calendar", { url, name }),
  addLocalCalendar: (name, color) => call("add_local_calendar", { name, color }),
  removeLocalCalendar: (calendarId) => call("remove_local_calendar", { calendarId }),
};

/** Subscribe to backend events (no-op in the browser mock). */
export function listen(event, fn) {
  window.__TAURI__?.event?.listen(event, (e) => fn(e.payload));
}
