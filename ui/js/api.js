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

/// "2026-11-08 1407" in local time, for exported file names.
function localStamp() {
  const d = new Date(), p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}${p(d.getMinutes())}`;
}

function indexOnAndroid(path) {
  try { if (path && window.CarteiroAndroid) window.CarteiroAndroid.indexFile(path); } catch {}
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
  // Saved into Downloads/Carteiro; on Android, also added to the media index
  // so the files show up in Files / Downloads.
  exportMail: () => call("export_mail", { stampLocal: localStamp() }).then((r) => (indexOnAndroid(r?.[0]), r)),
  saveAttachment: (messageId, index) => call("save_attachment", { messageId, index }).then((p) => (indexOnAndroid(p), p)),
  createBackup: (folder, passphrase, ui) => call("create_backup", { folder, passphrase, ui, stampLocal: localStamp() }).then((p) => (indexOnAndroid(p), p)),
  /** Sends the chosen backup file to the app in pieces, then restores it. */
  restoreBackup: async (file, passphrase, progress) => {
    if (!tauri) return { accounts: 1, ui: {} };
    const STEP = 3 << 20;
    for (let at = 0; at === 0 || at < file.size; at += STEP) {
      const piece = new Uint8Array(await file.slice(at, at + STEP).arrayBuffer());
      let bin = "";
      for (let i = 0; i < piece.length; i += 0x8000) bin += String.fromCharCode(...piece.subarray(i, i + 0x8000));
      await call("restore_upload", { data: btoa(bin), first: at === 0 });
      progress?.(Math.min(1, (at + STEP) / (file.size || 1)));
    }
    return call("restore_backup", { passphrase });
  },
  exportSettings: (ui) => call("export_settings", { ui, stampLocal: localStamp() }).then((p) => (indexOnAndroid(p), p)),
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
  listContacts: (query) => call("list_contacts", { query }),
  saveContact: (id, contact) => call("save_contact", { id, contact }),
  deleteContact: (id) => call("delete_contact", { id }),
  importContacts: (text) => call("import_contacts", { text }),
  exportContacts: () => call("export_contacts", { stampLocal: localStamp() }).then((p) => (indexOnAndroid(p), p)),
  contactSources: () => call("list_contact_sources"),
  addCarddav: (url, username, password, name) => call("add_carddav_contacts", { url, username, password, name }),
  removeContactSource: (id) => call("remove_contact_source", { id }),
  syncContacts: () => call("sync_contacts"),
  syncPhoneContacts: (contacts) => call("sync_phone_contacts", { contacts }),
  previewIcs: (messageId, index) => call("preview_ics", { messageId, index }),
  addIcs: (messageId, index) => call("add_ics", { messageId, index }),
  sendRsvp: (eventId) => call("send_rsvp", { eventId }),
  removeSeries: (eventId, from, to) => call("remove_series", { eventId, from, to }),
  setRsvp: (eventId, rsvp) => call("set_rsvp", { eventId, rsvp }),
  setUtcOffset: () => call("set_utc_offset", { minutes: -new Date().getTimezoneOffset() }),
  suggestAddresses: (prefix) => call("suggest_addresses", { prefix }),
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
