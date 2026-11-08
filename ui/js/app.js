// Main window: folders, conversation list, reading pane. Each region is
// re-rendered on its own so typing in search or compose never loses focus.

import { api, listen } from "./api.js";
import { $, choose, html, on, put, raw } from "./dom.js";
import { addrList, avatar, bytes, folderLabel, icon, longDate, shortDate } from "./format.js";
import { openCompose, openRules, openSettings, openSetup, ruleFromMessage } from "./dialogs.js";
import { eventWhen, refreshCalendar, rsvpButtons, showCalendar, showEventDay } from "./calendar.js";
import { leaveContacts, showContacts, syncPhoneContacts } from "./contacts.js";

export const S = {
  accounts: [], account: null, folders: [], folder: "INBOX", threads: [], selected: null, filter: "",
  messages: [], query: "", syncing: false, loading: false, drawer: false,
  expanded: {}, showQuoted: {}, plain: {}, light: {}, images: {}, menuFor: null,
  openThreads: {}, children: {}, focusMsg: null,
  // Conversations picked for a group action (press and hold, or Ctrl-click).
  // Messages of the open conversation that matched the search, and the
  // attachment types it asked for.
  hits: new Set(), hitExts: [],
  picked: new Set(),
  // Single messages picked inside a conversation (ref -> message ids), when
  // not all of it is picked.
  pickedMsgs: new Map(),
  // Account and folders of the conversation being read (differs from
  // S.account in the unified inbox).
  viewAcct: null, viewFolders: null,
};

/** Pseudo-folder: every account's Inbox merged. */
export const UNIFIED = "__all_inboxes__";

// Threads are addressed as "<account id>|<thread key>" so keys from different
// accounts never collide.
const refOf = (acct, key) => `${acct}|${key}`;
function splitRef(ref) {
  const i = ref.indexOf("|");
  return [+ref.slice(0, i), ref.slice(i + 1)];
}
export const accountById = (id) => S.accounts.find((a) => a.id === id);

// ---- helpers -------------------------------------------------------------

let toastTimer;
export function toast(msg) {
  put("#toast", msg);
  $("#toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => ($("#toast").hidden = true), 4000);
}

async function guard(p) {
  try { return await p; } catch (e) { toast(String(e)); }
}

// ---- Back (Android's back gesture, browser/mouse back) -----------------------------
// Each open layer adds a history entry, so Back closes the top layer instead of
// leaving the app. Layers closed by the UI itself drop their entry again.
const layers = [];
let ignorePops = 0;
const closers = {
  thread: () => { S.selected = null; S.messages = []; layoutState(); renderReader(); renderList(); },
  drawer: () => { S.drawer = false; layoutState(); },
  modal: () => put("#modal", ""),
  compose: () => put("#compose", ""),
  contact: () => document.dispatchEvent(new Event("contact-back")),
  jumper: () => document.dispatchEvent(new Event("jumper-back")),
  picks: () => { clearPicks(); renderList(); },
};
export function syncLayer(name, open) {
  const i = layers.lastIndexOf(name);
  if (open && i < 0) {
    layers.push(name);
    history.pushState({ layer: name }, "");
  } else if (!open && i >= 0) {
    layers.splice(i, 1);
    ignorePops++;
    history.back();
  }
}
// Android's back (see src-tauri/android/MainActivity.kt): close the top
// layer through its history entry; false when nothing is open (leave the app).
window.carteiroBack = () => {
  if (!layers.length) return false;
  history.back();
  return true;
};
window.addEventListener("popstate", () => {
  if (ignorePops) return void ignorePops--;
  const top = layers.pop();
  if (top) closers[top]();
});
for (const id of ["modal", "compose"]) {
  new MutationObserver(() => syncLayer(id, $(`#${id}`).childElementCount > 0)).observe($(`#${id}`), { childList: true });
}

function layoutState() {
  syncLayer("thread", !!S.selected);
  syncLayer("drawer", !!S.drawer);
  const app = $(".app");
  app.classList.toggle("has-sel", !!S.selected);
  app.classList.toggle("drawer-open", S.drawer);
  // Nothing to compose from until an account exists.
  app.classList.toggle("no-accounts", !S.accounts.length);
  for (const b of document.querySelectorAll("[data-click=sync]")) b.classList.toggle("spin", S.syncing);
}

// ---- theme -------------------------------------------------------------------

const THEME_KEY = "carteiro.theme";
const systemDark = window.matchMedia("(prefers-color-scheme: dark)");
function savedTheme() {
  try { return localStorage.getItem(THEME_KEY); } catch { return null; }
}
export const isDark = () => (savedTheme() ?? (systemDark.matches ? "dark" : "light")) === "dark";
function applyTheme() {
  document.documentElement.dataset.theme = isDark() ? "dark" : "light";
  for (const b of document.querySelectorAll("[data-click=theme] .theme-icon")) b.innerHTML = String(icon(isDark() ? "sun" : "moon"));
}
on("click", "theme", () => {
  try { localStorage.setItem(THEME_KEY, isDark() ? "light" : "dark"); } catch {}
  applyTheme();
  renderFolders();
  renderReader();
});
systemDark.addEventListener("change", () => { if (!savedTheme()) { applyTheme(); renderFolders(); renderReader(); } });
applyTheme();

// ---- window controls (the top bar is the title bar on desktop) ----------------

const appWindow = window.__TAURI__?.window?.getCurrentWindow?.();
if (appWindow && !/Android/i.test(navigator.userAgent)) {
  $(".win-controls").hidden = false;
  on("click", "win-min", () => appWindow.minimize());
  on("click", "win-max", () => appWindow.toggleMaximize());
  on("click", "win-close", () => appWindow.close());
  $(".topbar").addEventListener("dblclick", (e) => {
    if (e.target.closest("[data-tauri-drag-region]") === e.target) appWindow.toggleMaximize();
  });
}

// ---- account menu (person icon) -----------------------------------------------------

function renderAccountMenu() {
  const unified = S.folder === UNIFIED;
  put("#acct-menu", html`
    ${S.accounts.map((a) => html`<button role="menuitemradio" aria-checked="${!unified && a.id === S.account?.id}" class="${!unified && a.id === S.account?.id ? "on" : ""}" data-click="acct-pick" data-id="${a.id}">
      ${avatar(a.name || a.email, 28)}<span class="who"><strong class="ellipsis">${a.name}</strong><span class="muted small ellipsis">${a.email}</span></span></button>`)}
    ${S.accounts.length > 1 ? html`<button role="menuitemradio" aria-checked="${unified}" class="${unified ? "on" : ""}" data-click="acct-unified">
      <span class="all-icon">${icon("inbox", 18)}</span><span class="who"><strong>All accounts</strong><span class="muted small">Unified inbox</span></span></button>` : ""}
    <div class="sep"></div>
    <button data-click="add-account">${icon("plus", 16)} Add account…</button>
    <button data-click="accounts">${icon("person", 16)} Manage accounts…</button>`);
}

function closeAccountMenu() {
  const m = $("#acct-menu");
  if (m) m.hidden = true;
}

on("click", "acct-menu", (el, e) => {
  e.stopPropagation();
  const m = $("#acct-menu");
  if (!m.hidden) return closeAccountMenu();
  S.drawer = false;
  layoutState();
  renderAccountMenu();
  // Drop down under the button on desktop; CSS turns it into a bottom sheet on phones.
  const r = el.getBoundingClientRect();
  m.style.top = `${r.bottom + 4}px`;
  m.style.right = `${Math.max(8, window.innerWidth - r.right)}px`;
  m.hidden = false;
});
on("click", "acct-pick", (el) => {
  closeAccountMenu();
  const a = accountById(+el.dataset.id);
  if (a) {
    S.folder = "INBOX";
    selectAccount(a);
  }
});
on("click", "acct-unified", () => {
  closeAccountMenu();
  openFolder(UNIFIED);
});
document.addEventListener("click", (e) => {
  if (!e.target.closest("[data-click=acct-menu], #acct-menu")) closeAccountMenu();
});

// ---- mail / calendar switch ---------------------------------------------------------

on("click", "mode", (el) => {
  const cal = el.dataset.mode === "calendar", people = el.dataset.mode === "contacts";
  $(".app").classList.toggle("cal-mode", cal);
  $(".app").classList.toggle("contacts-mode", people);
  $("#calendar").hidden = !cal;
  $("#contacts").hidden = !people;
  for (const b of document.querySelectorAll("[data-click=mode]")) b.classList.toggle("on", b === el);
  S.drawer = false;
  layoutState();
  if (cal) showCalendar();
  if (people) showContacts();
  else leaveContacts();
});

// ---- data flow -------------------------------------------------------------

export async function loadAccounts() {
  S.accounts = (await guard(api.listAccounts())) ?? [];
  if (!S.accounts.length) {
    // Nothing of a removed account may stay on screen.
    Object.assign(S, { account: null, folders: [], threads: [], messages: [], selected: null, children: {}, openThreads: {}, viewAcct: null, viewFolders: null, folder: "INBOX" });
    layoutState();
    renderFolders();
    renderList();
    renderReader();
    // Shown at every start until an account exists, but always closable.
    return openSetup(true);
  }
  if (!S.account || !S.accounts.some((a) => a.id === S.account.id)) await selectAccount(S.accounts[0]);
}

export async function selectAccount(a) {
  S.account = a;
  if (S.folder !== UNIFIED) S.folder = "INBOX";
  S.selected = null;
  S.messages = [];
  await refreshFolders();
  await refreshThreads();
  renderReader();
  sync();
}

export async function refreshFolders() {
  if (S.account) S.folders = (await guard(api.folders(S.account.id))) ?? [];
  renderFolders();
}

// ---- filters (Outlook's Filter menu) and search syntax ---------------------------

const FILTERS = [["", "All"], ["is:unread", "Unread"], ["is:flagged", "Flagged"], ["to:me", "To me"], ["has:attachment", "Has attachments"]];

/// The search box plus the chosen filter, in the search syntax the engine parses.
const listQuery = () => [S.filter, S.query].filter(Boolean).join(" ") || null;

function renderFilter() {
  const label = FILTERS.find(([q]) => q === S.filter)?.[1];
  const bar = $("#filter-bar");
  bar.hidden = !S.filter;
  put("#filter-bar", S.filter ? html`<button class="chip-filter" data-click="set-filter" data-q="" title="Show all">${label} ${icon("close", 12)}</button>` : "");
  for (const b of document.querySelectorAll("[data-click=filter-menu]")) b.classList.toggle("on", !!S.filter);
}

on("click", "filter-menu", (el, e) => {
  e.stopPropagation();
  const m = $("#filter-menu");
  if (!m.hidden) return (m.hidden = true);
  put("#filter-menu", html`
    ${FILTERS.map(([q, l]) => html`<button class="${S.filter === q ? "on" : ""}" data-click="set-filter" data-q="${q}">${l}</button>`)}
    <p class="muted small hint">Search also understands <code>from:</code> <code>to:</code> <code>subject:</code> <code>has:attachment</code> <code>is:unread</code> <code>after:2026-10-01</code>, and searches the text of messages the virus scanner found clean.</p>`);
  const r = el.getBoundingClientRect();
  m.style.top = `${r.bottom + 4}px`;
  m.style.left = `${Math.max(8, Math.min(r.left, innerWidth - 300))}px`;
  m.hidden = false;
});
on("click", "set-filter", (el) => {
  S.filter = el.dataset.q;
  $("#filter-menu").hidden = true;
  renderFilter();
  refreshThreads();
});
document.addEventListener("click", (e) => {
  const m = $("#filter-menu");
  if (m && !m.hidden && !e.target.closest?.("#filter-menu, [data-click=filter-menu]")) m.hidden = true;
});

export async function refreshThreads() {
  if (S.folder === UNIFIED) {
    const list = (await guard(api.unified(listQuery()))) ?? [];
    S.threads = list.map((t) => ({ ...t, acct: t.account_id, ref: refOf(t.account_id, t.thread_key) }));
  } else if (S.account) {
    // Keep as many as were showing (older ones loaded by scrolling stay).
    const list = (await guard(api.threads(S.account.id, S.folder, listQuery(), 0, Math.max(100, S.threads.length)))) ?? [];
    S.threads = list.map((t) => ({ ...t, acct: S.account.id, folder: S.folder, ref: refOf(S.account.id, t.thread_key) }));
  }
  renderList();
}

async function sync() {
  if (!S.account || S.syncing) return;
  S.syncing = true;
  layoutState();
  const targets = S.folder === UNIFIED ? S.accounts : [S.account];
  let moved = 0, error = null;
  for (const a of targets) {
    const r = await guard(api.sync(a.id));
    moved += r?.moved_by_rules ?? 0;
    error ??= r?.errors[0] ?? null;
  }
  S.syncing = false;
  layoutState();
  if (error) toast(error);
  else if (moved) toast(`${moved} message(s) sorted by your rules`);
  await refreshFolders();
  await refreshThreads();
}

async function openFolder(name) {
  Object.assign(S, { folder: name, selected: null, messages: [], drawer: false });
  layoutState();
  renderFolders();
  renderReader();
  await refreshThreads();
}

async function openThread(ref, focusId = null) {
  const [acct, key] = splitRef(ref);
  S.selected = ref;
  S.viewAcct = acct;
  S.loading = true;
  layoutState();
  renderList();
  renderReader();
  S.viewFolders = acct === S.account?.id ? S.folders : ((await guard(api.folders(acct))) ?? []);
  S.messages = (await guard(api.openThread(acct, key))) ?? [];
  S.loading = false;
  S.expanded = {};
  S.menuFor = null;
  // Opened from a search: the messages that matched are opened, marked and
  // scrolled to (with their matching attachments highlighted).
  S.hits = new Set();
  S.hitExts = [];
  const query = listQuery();
  if (query) {
    const folder = S.threads.find((t) => t.ref === ref)?.folder ?? S.folder;
    const h = await guard(api.searchHits(acct, folder, key, query));
    S.hits = new Set(S.messages.filter((m) => h?.message_ids.includes(m.message_id)).map((m) => m.id));
    S.hitExts = h?.extensions ?? [];
    if (!focusId && S.hits.size) focusId = [...S.messages].reverse().find((m) => S.hits.has(m.id))?.id ?? null;
  }
  // The newest message is open; other unread ones too, unless there are so
  // many that the conversation would be a wall of text.
  const unseen = S.messages.filter((m) => !m.seen).length;
  S.messages.forEach((m, i) => (S.expanded[m.id] = (!S.hits.size && i === S.messages.length - 1) || (!m.seen && unseen <= 3) || m.id === focusId || S.hits.has(m.id)));
  S.focusMsg = focusId;
  S.children[ref] = S.messages;
  renderReader();
  renderList();
  if (focusId) document.getElementById(`msg-${focusId}`)?.scrollIntoView({ block: "start" });
  const unread = S.messages.filter((m) => !m.seen).map((m) => m.id);
  if (unread.length) {
    S.threads = S.threads.map((t) => (t.ref === ref ? { ...t, unread: 0 } : t));
    for (const m of S.messages) m.seen = true;
    renderReader();
    renderList();
    await guard(api.setFlag(acct, unread, "seen", true));
    refreshFolders();
  }
}

// Shows an action on the list at once, before the server has it.
function patchThreads(refs, kind) {
  const set = new Set(refs);
  if (kind === "delete" || kind === "move" || kind.startsWith("move-to:")) {
    S.threads = S.threads.filter((t) => !set.has(t.ref));
  } else {
    const change = { read: { unread: 0 }, flag: { flagged: true }, unflag: { flagged: false } }[kind];
    S.threads = S.threads.map((t) => !set.has(t.ref) ? t
      : kind === "unread" ? { ...t, unread: Math.max(1, t.unread) }
      : change ? { ...t, ...change } : t);
  }
  renderList();
}

async function act(kind, ids, dest, acct = S.viewAcct ?? S.account?.id) {
  if (acct == null || !ids.length) return;
  if (S.selected && S.messages.some((m) => ids.includes(m.id))) patchThreads([S.selected], kind);
  const id = acct;
  // The open conversation shows the change at once, not after a refresh.
  const field = { flag: ["flagged", true], unflag: ["flagged", false], read: ["seen", true], unread: ["seen", false] }[kind];
  if (field && S.messages.some((m) => ids.includes(m.id))) {
    for (const m of S.messages) if (ids.includes(m.id)) m[field[0]] = field[1];
    renderReader();
  }
  if (kind === "delete") await guard(api.remove(id, ids));
  else if (kind === "move") await guard(api.move(id, ids, dest));
  else if (kind === "read" || kind === "unread") await guard(api.setFlag(id, ids, "seen", kind === "read"));
  else if (kind === "flag" || kind === "unflag") await guard(api.setFlag(id, ids, "flagged", kind === "flag"));
  if (kind === "delete" || kind === "move") {
    S.selected = null;
    S.messages = [];
    layoutState();
    renderReader();
  }
  await refreshFolders();
  await refreshThreads();
}

// kind is "delete" | "read" | "flag" | "unflag" | "move-to:<folder>" (drag and drop).
// An action on a whole conversation, as Outlook does: read, unread and flags
// cover all its messages in every folder; delete and move every copy of
// them in the folder shown.
const EVERYWHERE = new Set(["read", "unread", "flag", "unflag"]);
async function threadAct(ref, kind) {
  patchThreads([ref], kind);
  const [acct, key] = splitRef(ref);
  const folder = S.threads.find((t) => t.ref === ref)?.folder ?? S.folder;
  const ids = (await guard(api.threadIds(acct, key, EVERYWHERE.has(kind) ? null : folder))) ?? [];
  if (kind.startsWith("move-to:")) await act("move", ids, kind.slice(8), acct);
  else await act(kind, ids, undefined, acct);
}

// ---- rendering -------------------------------------------------------------

const FOLDER_ICONS = ["inbox", "sent", "drafts", "trash", "junk", "archive"];

function renderFolders() {
  const multi = S.accounts.length > 1;
  put("#folders", html`
    <button class="btn primary new" data-click="compose">${icon("compose", 16)} New mail</button>
    ${S.folder === UNIFIED
      ? html`<div class="acct-name ellipsis">All accounts</div>`
      : S.account && html`<div class="acct-name ellipsis" title="${S.account.email}">${S.account.email}</div>`}
    <nav>
      ${S.folder === UNIFIED ? html`<button class="folder active" data-click="folder" data-name="${UNIFIED}">
          ${icon("inbox", 16)}<span class="label ellipsis">All inboxes</span></button><div class="nav-sep"></div>` : ""}
      ${S.folders.map((f) => {
        const depth = f.delimiter && !f.special ? f.name.split(f.delimiter).length - 1 : 0;
        return html`<button class="folder ${f.name === S.folder ? "active" : ""}" style="padding-left:${12 + depth * 14}px"
          data-click="folder" data-dragover="folderover" data-dragleave="folderleave" data-drop="folderdrop" data-name="${f.name}">
          ${icon(FOLDER_ICONS.includes(f.special) ? f.special : "folder", 16)}
          <span class="label ellipsis">${folderLabel(f)}</span>
          ${f.unread ? html`<span class="count">${f.unread}</span>` : ""}
        </button>`;
      })}
    </nav>
    <div class="foot mobile-only-block">
      <button class="folder" data-click="theme">${icon(isDark() ? "sun" : "moon", 16)}<span class="label">${isDark() ? "Light theme" : "Dark theme"}</span></button>
      <button class="folder" data-click="acct-menu">${icon("person", 16)}<span class="label">Accounts</span></button>
      <button class="folder" data-click="settings">${icon("settings", 16)}<span class="label">Settings</span></button>
    </div>`);
}

function renderList() {
  openTray = null; // the rows are drawn anew
  const f = S.folders.find((x) => x.name === S.folder);
  // Before folders load (or with no account) IMAP's "INBOX" reads as "Inbox".
  const title = S.folder === UNIFIED ? "All inboxes" : f ? folderLabel(f) : S.folder.toUpperCase() === "INBOX" ? "Inbox" : S.folder;
  put("#list-title", title);
  put("#mobile-title", title);
  put("#threads", S.threads.length ? S.threads.map((t) => html`
    <div class="item ${t.ref === S.selected ? "sel" : ""} ${t.unread ? "unread" : ""} ${S.picked.has(t.ref) ? "picked" : ""}" role="option"
      aria-selected="${picking() ? S.picked.has(t.ref) : t.ref === S.selected}" draggable="${FINE_POINTER && !picking()}"
      data-click="thread" data-dragstart="threaddrag" data-key="${t.ref}" data-latest="${t.latest_id}">
      <div class="tray" aria-hidden="true">${trayHtml(t)}</div>
      <div class="pick-hint" aria-hidden="true">${icon("check", 20)}<span>Select</span></div>
      <div class="item-body">
      ${t.count > 1
        ? html`<button class="chevron ${S.openThreads[t.ref] ? "open" : ""}" title="Show messages" aria-expanded="${!!S.openThreads[t.ref]}" data-click="thread-expand">${icon("chevron", 14)}</button>`
        : html`<span class="chevron-gap"></span>`}
      ${picking() ? pickBox(S.picked.has(t.ref), S.pickedMsgs.has(t.ref)) : ""}
      ${avatar(t.participants[0] ?? "?")}
      <div class="main">
        <div class="row"><span class="who ellipsis">${t.participants.join(", ")}</span>
          ${t.count > 1 ? html`<span class="count">${t.count}</span>` : ""}</div>
        <div class="row"><span class="subject ellipsis">${t.subject || "(no subject)"}</span>
          ${t.has_attachments ? icon("clip", 13) : ""}
          ${t.flagged ? html`<span class="flagged">${icon("flag", 13)}</span>` : ""}
          <span class="date">${shortDate(t.last_date)}</span></div>
        <div class="snippet ellipsis">${t.snippet}</div>
        ${S.folder === UNIFIED && S.accounts.length > 1 ? html`<div class="acct-tag ellipsis">${t.account_email}</div>` : ""}
      </div>
      <div class="hover-actions">
        <button class="icon-btn" title="Delete" data-click="thread-act" data-kind="delete">${icon("trash", 15)}</button>
        <button class="icon-btn" title="${t.unread ? "Mark as read" : "Mark unread"}" data-click="thread-act" data-kind="${t.unread ? "read" : "unread"}">${icon("read", 15)}</button>
        <button class="icon-btn ${t.flagged ? "on" : ""}" title="${t.flagged ? "Clear flag" : "Flag"}" data-click="thread-act" data-kind="${t.flagged ? "unflag" : "flag"}">${icon("flag", 15)}</button>
      </div>
      </div>
    </div>
    ${S.openThreads[t.ref] ? threadChildren(t.ref, t.folder) : ""}`) : !S.accounts.length
      ? html`<div class="empty muted"><p>No email account yet.</p><button class="btn primary" data-click="add-account">Add an account</button><p><button class="linkish" data-click="restore-open">or restore from a backup</button></p></div>`
      : html`<div class="empty muted">Nothing here. Enjoy the quiet.</div>`);
  renderPickBar();
}

// Rows drag to folders only with a mouse: on touch screens, Android's web
// view would start a drag on a long press and swallow the hold.
const FINE_POINTER = matchMedia("(pointer: fine)").matches;

// ---- picking several conversations -------------------------------------------------

function renderPickBar() {
  const shown = (r) => S.threads.some((t) => t.ref === r);
  S.picked = new Set([...S.picked].filter(shown));
  for (const r of S.pickedMsgs.keys()) if (!shown(r)) S.pickedMsgs.delete(r);
  const bar = $("#pick-bar");
  bar.hidden = !picking();
  syncLayer("picks", picking());
  if (!picking()) return put(bar, "");
  const folders = (S.viewFolders ?? S.folders).filter((f) => f.special !== "outbox" && f.name !== S.folder);
  put(bar, html`
    <button class="icon-btn" title="Stop picking" aria-label="Stop picking" data-click="pick-clear">${icon("close", 18)}</button>
    <span class="pick-count">${pickLabel()}</span>
    <span class="spacer"></span>
    <button class="icon-btn" title="Mark as read" data-click="pick-act" data-kind="read">${icon("read", 18)}</button>
    <button class="icon-btn" title="Mark unread" data-click="pick-act" data-kind="unread">${icon("compose", 18)}</button>
    <button class="icon-btn" title="Flag" data-click="pick-act" data-kind="flag">${icon("flag", 18)}</button>
    <button class="icon-btn" title="Delete" data-click="pick-act" data-kind="delete">${icon("trash", 18)}</button>
    ${S.folder === UNIFIED ? "" : html`<select class="move" title="Move to" aria-label="Move to" data-change="pick-move">
      <option value="">Move…</option>${folders.map((f) => html`<option value="${f.name}">${folderLabel(f)}</option>`)}</select>`}`);
}

const picking = () => S.picked.size > 0 || S.pickedMsgs.size > 0;
function clearPicks() { S.picked.clear(); S.pickedMsgs.clear(); }
// A check box: ticked, partly ticked (some messages of the conversation), or empty.
const pickBox = (on, partly = false) => html`<span class="pick-box ${on ? "on" : partly ? "partly" : ""}" role="checkbox" aria-checked="${on ? "true" : partly ? "mixed" : "false"}">${on ? icon("check", 16) : partly ? html`<span class="dash"></span>` : ""}</span>`;
function pickLabel() {
  const convs = S.picked.size, msgs = [...S.pickedMsgs.values()].reduce((n, s) => n + s.size, 0);
  const m = `${msgs} message${msgs > 1 ? "s" : ""}`;
  return convs && msgs ? `${convs} + ${m}` : convs ? `${convs} selected` : m;
}

// Picking a conversation picks all of it; one with several messages opens in
// the list so each message shows its own (ticked) box.
async function togglePick(ref) {
  if (S.picked.has(ref) || S.pickedMsgs.has(ref)) {
    S.picked.delete(ref);
    S.pickedMsgs.delete(ref);
    return renderList();
  }
  S.picked.add(ref);
  const t = S.threads.find((x) => x.ref === ref);
  if (t?.count > 1 && !S.openThreads[ref]) {
    S.openThreads[ref] = true;
    if (!S.children[ref]) {
      renderList();
      S.children[ref] = (await guard(api.openThread(...splitRef(ref)))) ?? [];
    }
  }
  renderList();
}

// Unticking one message of a picked conversation keeps the others picked.
function toggleMsgPick(ref, id) {
  const all = (S.children[ref] ?? []).map((m) => m.id);
  let set = S.picked.has(ref) ? new Set(all) : new Set(S.pickedMsgs.get(ref) ?? []);
  S.picked.delete(ref);
  if (set.has(id)) set.delete(id); else set.add(id);
  if (all.length && all.every((x) => set.has(x))) { S.pickedMsgs.delete(ref); S.picked.add(ref); }
  else if (set.size) S.pickedMsgs.set(ref, set);
  else S.pickedMsgs.delete(ref);
  renderList();
}

async function actOnPicked(kind) {
  const refs = [...S.picked];
  const msgs = [...S.pickedMsgs];
  clearPicks();
  patchThreads(refs, kind);
  for (const r of refs) await threadAct(r, kind);
  // Single messages: just those (the copies shown).
  for (const [ref, ids] of msgs) {
    const [acct] = splitRef(ref);
    if (kind.startsWith("move-to:")) await act("move", [...ids], kind.slice(8), acct);
    else await act(kind, [...ids], undefined, acct);
    delete S.children[ref];
  }
  for (const r of refs) delete S.children[r];
  if (msgs.length) await refreshThreads();
}
on("click", "pick-clear", () => { clearPicks(); renderList(); });
on("click", "pick-act", (el) => actOnPicked(el.dataset.kind));
on("change", "pick-move", (el) => el.value && actOnPicked(`move-to:${el.value}`));

// ---- holding and swiping a conversation ------------------------------------------
//
// Touch: hold to pick; swipe right to pick; swipe left to open a tray of up to
// four actions (chosen in Settings). Once a gesture is sideways it only moves
// the row sideways (the list doesn't scroll). Mouse: Ctrl/Cmd-click picks, and
// holding the button down works too.

export const SWIPE_ACTIONS = [["read", "Read / unread"], ["flag", "Flag / unflag"], ["move", "Move"], ["archive", "Archive"], ["delete", "Delete"]];
const SWIPE_DEFAULT = ["read", "flag", "archive", "delete"];
export function swipeActions() {
  try {
    const v = JSON.parse(localStorage.getItem("carteiro.swipe.actions"));
    if (Array.isArray(v)) return v.filter((k) => SWIPE_ACTIONS.some(([x]) => x === k)).slice(0, 4);
  } catch {}
  return SWIPE_DEFAULT;
}
const TRAY_W = 72;
// What a tray button does to conversation t, and how it looks.
function trayButton(kind, t) {
  if (kind === "read") return t.unread ? ["read", "Read", "read", "#0f6cbd"] : ["unread", "Unread", "compose", "#0f6cbd"];
  if (kind === "flag") return t.flagged ? ["unflag", "Unflag", "flag", "#ca5010"] : ["flag", "Flag", "flag", "#ca5010"];
  return { move: ["move", "Move", "folder", "#5c2e91"], archive: ["archive", "Archive", "archive", "#107c10"], delete: ["delete", "Delete", "trash", "#c50f1f"] }[kind];
}
function trayHtml(t) {
  return html`${swipeActions().map((k) => {
    const [act, label, ic, color] = trayButton(k, t);
    return html`<button class="tray-btn" style="background:${color}" data-click="tray" data-kind="${act}">${icon(ic, 18)}<span>${label}</span></button>`;
  })}`;
}

// A tap right after a hold or swipe belongs to that gesture, not a new one.
let press = null, quietUntil = 0, openTray = null;
// After a hold or swipe, taps are ignored until the next touch begins (the
// browser's tap on lifting the finger can come late, and its touchend can be
// lost when the row under the finger is redrawn).
const quiet = () => { quietUntil = Infinity; };
const HOLD_MS = 450;
const rowOf = (target) => target.closest?.("#threads .item");
const bodyOf = (item) => item.querySelector(".item-body");

function closeTray() {
  if (!openTray) return;
  const b = bodyOf(openTray);
  if (b) b.style.transform = "";
  openTray.classList.remove("tray-open");
  openTray = null;
}

function startPress(item, x, y, touch) {
  if (openTray && openTray !== item) closeTray();
  press = { item, x, y, touch, axis: null, dx: 0, base: item === openTray ? -trayWidth(item) : 0,
    timer: setTimeout(() => {
      if (!press || press.axis) return;
      press.held = true;
      if (!press.touch) quietUntil = Date.now() + 400; else quiet();
      closeTray();
      togglePick(item.dataset.key);
      navigator.vibrate?.(15);
    }, HOLD_MS) };
}
const trayWidth = (item) => item.querySelectorAll(".tray-btn").length * TRAY_W;

// Returns true when the gesture is sideways (the caller then stops scrolling).
function movePress(x, y) {
  if (!press) return false;
  const dx = x - press.x, dy = y - press.y;
  if (!press.axis && Math.hypot(dx, dy) > 10) {
    clearTimeout(press.timer);
    press.axis = press.touch && Math.abs(dx) > Math.abs(dy) && !picking() ? "x" : "y";
    if (press.axis === "x") press.item.classList.add("swiping");
  }
  if (press.axis !== "x") return false;
  const max = trayWidth(press.item);
  // Left reveals the tray (a little give past it); right pulls toward picking.
  const pos = Math.max(-max - 24, Math.min(press.item.offsetWidth * 0.5, press.base + dx));
  press.dx = pos;
  bodyOf(press.item).style.transform = `translateX(${pos}px)`;
  press.item.classList.toggle("pick-armed", pos > press.item.offsetWidth * 0.25);
  press.item.classList.toggle("tray-showing", pos < 0);
  return true;
}

function endPress() {
  if (!press) return;
  clearTimeout(press.timer);
  const p = press;
  press = null;
  // The tap the browser makes on lifting the finger isn't a new tap.
  if (p.held) return p.touch ? quiet() : void (quietUntil = Date.now() + 400);
  if (p.axis !== "x") return;
  quiet();
  p.item.classList.remove("swiping", "pick-armed");
  const body = bodyOf(p.item);
  if (p.dx > p.item.offsetWidth * 0.25) {
    body.style.transform = "";
    p.item.classList.remove("tray-showing");
    return togglePick(p.item.dataset.key);
  }
  const max = trayWidth(p.item);
  if (p.dx < -Math.min(48, max / 2) && max) {
    body.style.transform = `translateX(${-max}px)`;
    p.item.classList.add("tray-open", "tray-showing");
    openTray = p.item;
  } else {
    body.style.transform = "";
    p.item.classList.remove("tray-showing");
    if (openTray === p.item) openTray = null;
  }
}

// Touch events, so a sideways swipe can stop the list from scrolling. The
// rest of a touch is followed on the element it started on: picking redraws
// the list, and a removed element still gets its touch, but nothing above it does.
let lastTouch = 0;
document.addEventListener("touchstart", (e) => {
  quietUntil = 0; // a new touch: taps count again
  const item = rowOf(e.target);
  if (!item || e.touches.length > 1 || e.target.closest("select, .tray-btn")) return;
  startPress(item, e.touches[0].clientX, e.touches[0].clientY, true);
  const target = e.target;
  const move = (ev) => { if (press?.touch && movePress(ev.touches[0].clientX, ev.touches[0].clientY)) ev.preventDefault(); };
  const end = (ev) => {
    target.removeEventListener("touchmove", move);
    target.removeEventListener("touchend", end);
    target.removeEventListener("touchcancel", end);
    lastTouch = Date.now();
    if (!press?.touch) return;
    if (ev.type === "touchcancel") press.dx = 0;
    endPress();
  };
  target.addEventListener("touchmove", move, { passive: false });
  target.addEventListener("touchend", end);
  target.addEventListener("touchcancel", end);
}, { passive: true });
// Mouse: holding the button down picks too.
document.addEventListener("mousedown", (e) => {
  const item = rowOf(e.target);
  // (browsers imitate mouse events after a touch: those aren't presses)
  if (item && e.button === 0 && !e.target.closest("button, select") && Date.now() - lastTouch > 800 && !e.sourceCapabilities?.firesTouchEvents) startPress(item, e.clientX, e.clientY, false);
});
document.addEventListener("mousemove", (e) => { if (press && !press.touch) movePress(e.clientX, e.clientY); });
document.addEventListener("mouseup", () => { if (press && !press.touch) endPress(); });
// Holding a conversation picks it; no text selection or menu from the system.
document.addEventListener("contextmenu", (e) => { if (rowOf(e.target)) e.preventDefault(); });
// A hold or a swipe isn't also a tap; a tap elsewhere closes an open tray.
document.addEventListener("click", (e) => {
  // Wherever it lands: the selection bar may have appeared under the finger.
  if (Date.now() < quietUntil) { e.stopPropagation(); e.preventDefault(); }
  else if (openTray && !e.target.closest?.(".tray-btn")) {
    const inside = e.target.closest?.("#threads .item") === openTray;
    closeTray();
    if (inside) { e.stopPropagation(); e.preventDefault(); }
  }
}, true);

on("click", "tray", async (el, e) => {
  e.stopPropagation();
  const ref = el.closest("[data-key]").dataset.key;
  const kind = el.dataset.kind;
  closeTray();
  if (kind === "archive") {
    const archive = (S.viewFolders ?? S.folders).find((f) => f.special === "archive")?.name;
    return archive ? threadAct(ref, `move-to:${archive}`) : toast("This account has no Archive folder");
  }
  if (kind === "move") {
    const folders = (S.viewFolders ?? S.folders).filter((f) => f.special !== "outbox" && f.name !== (S.threads.find((t) => t.ref === ref)?.folder ?? S.folder));
    const r = await choose("Move to which folder?", folders.map((f) => [f.name, folderLabel(f)]));
    return r && threadAct(ref, `move-to:${r.choice}`);
  }
  threadAct(ref, kind);
});

// Outlook-style expanded conversation: one row per message, newest first.
function threadChildren(key, folder) {
  const msgs = S.children[key];
  if (!msgs) return html`<div class="child muted">Loading…</div>`;
  return [...msgs].reverse().map((m) => html`
    <div class="child ${m.id === S.focusMsg && key === S.selected ? "sel" : ""} ${m.seen ? "" : "unread"} ${picking() ? "picking" : ""}"
      data-click="thread-msg" data-key="${key}" data-id="${m.id}">
      ${picking() ? pickBox(S.picked.has(key) || !!S.pickedMsgs.get(key)?.has(m.id)) : ""}
      <div class="child-main">
      <div class="row"><span class="who ellipsis">${m.from.name || m.from.email}</span>
        ${m.folder !== folder ? html`<span class="folder-tag">${folderName(m.folder)}</span>` : ""}
        <span class="date">${shortDate(m.date)}</span></div>
      <div class="snippet ellipsis">${m.snippet}</div>
      </div>
    </div>`);
}

function folderName(name) {
  const f = (S.viewFolders ?? S.folders).find((x) => x.name === name);
  return f ? folderLabel(f) : name;
}

const MAIL_CSP = "default-src 'none'; img-src data: cid:; style-src 'unsafe-inline'; font-src data:";
// HTML mail goes in a sandboxed iframe: scripts off, remote content blocked
// by CSP (no tracking pixels). allow-same-origin only lets us measure height.
// Quoted history as the common mail apps mark it: <blockquote> (Apple Mail and
// most others), Gmail, Outlook's reply header and all that follows, Yahoo, Thunderbird.
const QUOTE_SEL = "blockquote,.gmail_quote,.yahoo_quoted,.moz-cite-prefix,#divRplyFwdMsg,#divRplyFwdMsg~*,#appendonsend~*";
const hasQuote = (h) => /<blockquote|gmail_quote|yahoo_quoted|moz-cite-prefix|divRplyFwdMsg|appendonsend/i.test(h);
// Quote levels (parent first) whose messages aren't in this conversation stay
// visible; deeper ones, and all of them when the parent is here, are hidden.
const MAX_LEVEL = 4294967295;
function hideQuotes(missing) {
  if (missing === MAX_LEVEL) return "";
  if (!missing) return `${QUOTE_SEL}{display:none!important}`;
  const nested = (sel) => `${`${sel} `.repeat(missing)}${sel}`;
  return `${nested("blockquote")},${nested(".gmail_quote")}{display:none!important}`;
}

/// Plain text: the quoted part down to the first level we already have
/// ("> " depth), without the attribution line that introduces it.
function missingQuote(text, missing) {
  if (missing === MAX_LEVEL) return text;
  const depth = (l) => (l.match(/^(\s*>)*/)[0].match(/>/g) ?? []).length;
  const attribution = /(wrote|escreveu|escribió|a écrit|schrieb|ha scritto):\s*$|^-+\s*original message\s*-+$/i;
  const out = [];
  for (const line of text.split("\n")) {
    const d = depth(line);
    if (d > missing) continue;
    if (d === missing && attribution.test(line.replace(/^(\s*>)*\s*/, ""))) continue;
    out.push(line);
  }
  while (out.length && /^[\s>]*$/.test(out[out.length - 1])) out.pop();
  return out.join("\n").trim();
}

// Dark theme: invert the mail's colours keeping hues (white -> the card's dark
// grey), and invert images back so photos and logos look as sent.
const DARK_MAIL = "html{filter:invert(1) hue-rotate(180deg)}body{background:#d6d6d6}img,video,picture,svg,[style*=background-image],[background]{filter:invert(1) hue-rotate(180deg)}";

// "Load images": only https images from hosts that aren't trackers; tracker
// images and 1x1 pixels (open tracking from any host) are removed first.
// DOMParser documents are inert: nothing loads or runs while we look.
function withImages(m) {
  const blocked = new Set(m.remote_images.filter((i) => i.tracker).map((i) => i.url));
  const doc = new DOMParser().parseFromString(m.body_html, "text/html");
  for (const img of doc.querySelectorAll("img")) {
    const src = img.getAttribute("src") ?? "";
    const tiny = ["width", "height"].some((a) => /^\s*[0-2](px)?\s*$/i.test(img.getAttribute(a) ?? ""));
    if (blocked.has(src) || tiny) img.remove();
  }
  // Plain host names only: they go into the frame's Content-Security-Policy.
  const hosts = [...new Set(m.remote_images.filter((i) => !i.tracker && i.url.startsWith("https://") && /^[a-z0-9.-]+$/.test(i.host)).map((i) => `https://${i.host}`))];
  return { html: doc.body.innerHTML, imgSrc: hosts.join(" ") };
}

function mailFrame(m) {
  const dark = isDark() && !S.light[m.id];
  const css = `:root{color-scheme:light only}body{margin:0;font:14px/1.5 "Segoe UI",system-ui,sans-serif;overflow-wrap:anywhere;color:#242424;background:#fff}img{max-width:100%;height:auto}table{max-width:100%}`
    + (dark ? DARK_MAIL : "") + (S.showQuoted[m.id] ? "" : hideQuotes(m.quote_missing));
  const shown = S.images[m.id] ? withImages(m) : { html: m.body_html, imgSrc: "" };
  const csp = shown.imgSrc ? MAIL_CSP.replace("img-src data: cid:", `img-src data: cid: ${shown.imgSrc}`) : MAIL_CSP;
  return html`<iframe class="mailbody" title="Message body"
  sandbox="allow-same-origin"
  srcdoc="${`<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${csp}"><meta name="color-scheme" content="light only"><base target="_blank"><style>${css}</style></head><body>${shown.html}</body></html>`}"></iframe>`;
}

// Quoted text that's only a copy of messages in this conversation is folded;
// parts quoting messages we don't have stay visible.
function quotedPlain(m) {
  const partial = m.quote_missing ? missingQuote(m.quoted_text, m.quote_missing) : "";
  const full = S.showQuoted[m.id];
  const more = partial !== m.quoted_text.trim();
  return html`
    ${!full && partial ? html`<p class="muted small quote-note">Quoting earlier mail that isn't in this conversation:</p><pre class="text quoted">${partial}</pre>` : ""}
    ${full ? html`<pre class="text quoted">${m.quoted_text}</pre>` : ""}
    ${more || full ? html`<button class="linkish" data-click="quoted" data-id="${m.id}">${full ? "Hide" : "Show"} ${partial ? "full " : ""}quoted text</button>` : ""}`;
}

function imagesBanner(m) {
  const trackers = m.remote_images.filter((i) => i.tracker).length;
  const loadable = m.remote_images.length - trackers;
  return html`<div class="banner info images">${icon("shield", 16)}<div>
    Images from the web are blocked to protect your privacy${trackers ? html` (${trackers} from trackers)` : ""}.
    ${loadable ? html`<button class="linkish" data-click="load-images" data-id="${m.id}">Load images</button>${trackers ? " (trackers stay blocked)" : ""}` : ""}
  </div></div>`;
}

// Whether ClamAV has checked this message (and with it, its attachments).
function scanBadge(m) {
  const v = m.verdict;
  if (v.status === "clean") return html`<span class="scan ok" title="ClamAV checked this message and its attachments: nothing found">${icon("shield", 12)} Scanned</span>`;
  if (v.status === "infected") return html`<span class="scan bad" title="ClamAV found ${v.signature}">${icon("shield", 12)} Malware</span>`;
  if (v.reason === "scanning disabled") return html`<span class="scan off" title="Virus scanning is turned off in Settings">${icon("shield", 12)} Not scanned</span>`;
  return html`<span class="scan wait" title="Not checked yet: ${v.reason}">${icon("shield", 12)} Not scanned yet</span>`;
}

function renderMessage(m) {
  const open = S.expanded[m.id];
  const name = m.from.name || m.from.email;
  // Messages with deceptive links open as plain text, so real addresses show.
  const suspicious = (m.warnings ?? []).some((w) => w.startsWith("A link"));
  const showHtml = m.body_html && !(S.plain[m.id] ?? suspicious);
  return html`
    <article id="msg-${m.id}" class="card ${open ? "" : "collapsed"} ${m.id === S.focusMsg ? "focus" : ""} ${S.hits?.has(m.id) ? "hit" : ""}">
      ${S.hits?.has(m.id) ? html`<div class="hit-tag">${icon("search", 13)} Matches your search</div>` : ""}
      <div class="msg-head" data-click="toggle-msg" data-id="${m.id}">
        ${avatar(name, 36)}
        <div class="who">
          <div><strong>${name}</strong> ${open && m.from.name ? html`<span class="muted email">&lt;${m.from.email}&gt;</span>` : ""}</div>
          <div class="muted small ellipsis">${open ? `To: ${addrList(m.to)}${m.cc.length ? `; Cc: ${addrList(m.cc)}` : ""}` : m.snippet}</div>
        </div>
        <div class="when muted small" title="${longDate(m.date)}">${shortDate(m.date)}${open ? scanBadge(m) : ""}</div>
        ${open ? html`<div class="msg-actions">
          <button class="icon-btn" title="Reply" data-click="reply" data-id="${m.id}">${icon("reply")}</button>
          <button class="icon-btn hide-narrow" title="Reply all" data-click="reply-all" data-id="${m.id}">${icon("replyall")}</button>
          <button class="icon-btn hide-narrow" title="Forward" data-click="forward" data-id="${m.id}">${icon("forward")}</button>
          <button class="icon-btn" title="More" data-click="msg-menu" data-id="${m.id}">${icon("more")}</button>
          ${S.menuFor === m.id ? html`<div class="menu" role="menu">
            <button data-click="make-rule" data-id="${m.id}">Create rule…</button>
            <button data-click="msg-delete" data-id="${m.id}">Delete this message</button>
            <button data-click="msg-unread" data-id="${m.id}">Mark as unread</button>
            <button class="show-narrow" data-click="forward" data-id="${m.id}">Forward</button>
          </div>` : ""}
        </div>` : ""}
      </div>
      ${open ? html`
        ${m.verdict.status === "infected" ? html`<div class="banner danger">${icon("shield", 16)} Malware detected (${m.verdict.signature}). Formatted content and attachments are blocked.</div>` : ""}
        ${m.verdict.status === "not_scanned" && m.attachments_blocked && m.verdict.reason !== "scanning disabled" ? html`<div class="banner info">${icon("shield", 16)} Not scanned yet (${m.verdict.reason}). Showing plain text only; formatted content and attachments appear once the scan can run.</div>` : ""}
        ${showHtml && m.remote_images.length && !S.images[m.id] ? imagesBanner(m) : ""}
        ${(m.invites ?? []).map(inviteCard)}
        ${(m.warnings ?? []).length ? html`<div class="banner warn">${icon("junk", 16)}<div><strong>Be careful with this message</strong><ul>${m.warnings.map((w) => html`<li>${w}</li>`)}</ul></div></div>` : ""}
        <div class="msg-body">
          ${showHtml
            ? html`${mailFrame(m)}<div class="frame-actions">
                ${hasQuote(m.body_html) ? html`<button class="linkish" data-click="quoted" data-id="${m.id}">${S.showQuoted[m.id] ? "Hide" : "Show"} quoted text</button>` : ""}
                ${isDark() ? html`<button class="linkish" data-click="light" data-id="${m.id}">${S.light[m.id] ? "Dark background" : "Light background"}</button>` : ""}
                <button class="linkish" data-click="plain" data-id="${m.id}">Show plain text</button></div>`
            : html`<pre class="text">${m.body_text}</pre>
              ${m.quoted_text ? quotedPlain(m) : ""}
              ${m.body_html ? html`<button class="linkish" data-click="plain" data-id="${m.id}">Show formatted</button>` : ""}`}
        </div>
        ${m.attachments.length ? html`<div class="attachments">${m.attachments.map((a, i) => html`<div class="att ${a.danger ? "danger" : m.attachments_blocked ? "locked" : "savable"} ${S.hits?.has(m.id) && isHitAttachment(a) ? "hit" : ""} ${saving.has(`${m.id}/${i}`) ? "busy" : saved.has(`${m.id}/${i}`) ? "saved" : ""}" title="${a.danger ?? (m.verdict.status === "infected" ? `Blocked: ClamAV found ${m.verdict.signature}` : m.attachments_blocked ? "Blocked until ClamAV has checked this message" : `Checked by ClamAV. Save ${a.name} to Downloads (it's scanned again on its own first)`)}"
          ${a.danger || m.attachments_blocked ? "" : raw(`role="button" tabindex="0" data-click="save-att" data-id="${m.id}" data-index="${i}"`)}>
          ${icon(a.danger ? "junk" : "clip", 14)} <span class="ellipsis">${a.name}</span> <span class="muted small">${bytes(a.size)}</span>
          ${a.danger ? html`<span class="tag">dangerous type</span>` : m.verdict.status === "infected" ? html`<span class="tag">malware</span>` : m.attachments_blocked ? html`<span class="tag muted">not scanned yet</span>` : html`<span class="tag ok">${saving.has(`${m.id}/${i}`) ? "checking and saving…" : saved.has(`${m.id}/${i}`) ? "saved" : "scanned"}</span>`}</div>`)}</div>` : ""}
      ` : ""}
    </article>`;
}

// An invitation from a clean message, already in the Invitations calendar.
function inviteCard(e) {
  return html`<div class="invite">
    <div class="invite-head">${icon("calendar", 18)}<div>
      <strong>${e.title || "(no title)"}</strong>
      <div class="muted small">${eventWhen(e)}${e.recurring ? " · repeats" : ""}${e.location ? ` · ${e.location}` : ""}</div>
    </div></div>
    <div class="invite-foot">${rsvpButtons(e, true)}<span class="muted small">In your Invitations calendar. Your answer stays here unless you press Send answer.</span></div>
  </div>`;
}
document.addEventListener("rsvp-changed", ({ detail }) => {
  for (const m of S.messages) for (const e of m.invites ?? []) if (e.id === detail.id) e.rsvp = detail.rsvp;
});

// An attachment the search was about: its type was asked for (has:pdf), or
// its name contains a searched word.
function isHitAttachment(a) {
  const ext = (a.name.split(".").pop() ?? "").toLowerCase();
  if (S.hitExts.includes(ext)) return true;
  const words = (S.query ?? "").toLowerCase().split(/\s+/).filter((w) => w && !w.includes(":"));
  return words.some((w) => a.name.toLowerCase().includes(w));
}

function renderReader() {
  const ms = S.messages;
  if (!ms.length) {
    put("#reader", S.loading
      ? html`<div class="placeholder muted">Loading…</div>`
      : html`<div class="placeholder muted">${icon("read", 48)}<p>Select a conversation to read</p></div>`);
    return;
  }
  const last = ms[ms.length - 1];
  const subject = ms[0].subject.replace(/^((re|fw|fwd|aw|res|enc)(\[\d+\])?:\s*)+/i, "");
  put("#reader", html`
    <div class="pane">
      <header class="toolbar">
        <button class="icon-btn show-narrow" aria-label="Back" data-click="back">${icon("back", 22)}</button>
        <div class="spacer"></div>
        <button class="icon-btn" title="Delete" data-click="conv" data-kind="delete">${icon("trash")}</button>
        <button class="icon-btn" title="Archive" data-click="conv" data-kind="archive">${icon("archive")}</button>
        ${ms.some((m) => !m.seen)
          ? html`<button class="icon-btn" title="Mark as read" data-click="conv" data-kind="read">${icon("read")}</button>`
          : html`<button class="icon-btn" title="Mark unread" data-click="conv" data-kind="unread">${icon("read")}</button>`}
        ${ms.some((m) => m.flagged)
          ? html`<button class="icon-btn on" title="Clear flag" data-click="conv" data-kind="unflag">${icon("flag")}</button>`
          : html`<button class="icon-btn" title="Flag" data-click="conv" data-kind="flag">${icon("flag")}</button>`}
        <select class="move" title="Move to" data-change="move-to">
          <option value="">Move to…</option>
          ${(S.viewFolders ?? S.folders).filter((f) => f.name !== last.folder).map((f) => html`<option value="${f.name}">${folderLabel(f)}</option>`)}
        </select>
      </header>
      <h1 class="subject">${subject || "(no subject)"}</h1>
      <!-- Newest first, so replying to it sits at the top. -->
      <div class="quick-reply">
        <button class="btn" data-click="reply" data-id="${last.id}">${icon("reply", 15)} Reply</button>
        <button class="btn" data-click="reply-all" data-id="${last.id}">${icon("replyall", 15)} Reply all</button>
        <button class="btn" data-click="forward" data-id="${last.id}">${icon("forward", 15)} Forward</button>
      </div>
      ${[...ms].reverse().map(renderMessage)}
    </div>`);
  for (const f of document.querySelectorAll("iframe.mailbody")) {
    f.addEventListener("load", () => {
      // Scripts never run inside mail; the app handles link clicks itself and
      // opens them in the system browser (http/https/mailto only).
      const onLink = (e) => {
        const a = e.target.closest?.("a[href]");
        if (!a) return;
        e.preventDefault();
        const href = a.getAttribute("href").trim();
        if (/^(https?:|mailto:)/i.test(href)) guard(api.openLink(href));
      };
      try {
        f.contentDocument.addEventListener("click", onLink);
        f.contentDocument.addEventListener("auxclick", onLink);
      } catch {}
      try { f.style.height = Math.min(f.contentDocument.documentElement.scrollHeight + 16, 20000) + "px"; }
      catch { f.style.height = "600px"; }
    });
  }
}

// ---- reply / forward ---------------------------------------------------------

// Reply/forward subjects: line breaks (hidden in an encoded subject) would be
// refused when sending, so they become spaces.
const prefixed = (p, s) => { s = s.replace(/[\r\n]+/g, " "); return new RegExp(`^${p}:`, "i").test(s) ? s : `${p}: ${s}`; };
const quoteBody = (m) => `\n\n-----\nOn ${longDate(m.date)}, ${m.from.name || m.from.email} wrote:\n` + m.body_text.split("\n").map((l) => `> ${l}`).join("\n");
const msgById = (el) => S.messages.find((m) => m.id === +el.dataset.id);

/**
 * The account to answer from: whichever of the user's addresses the latest
 * message in the thread was sent to (or from), else the thread's account.
 */
function replyAccount() {
  const latest = S.messages[S.messages.length - 1];
  const addrs = latest ? [...latest.to, ...latest.cc, latest.from].map((a) => a.email.toLowerCase()) : [];
  return S.accounts.find((a) => addrs.includes(a.email.toLowerCase())) ?? accountById(S.viewAcct) ?? S.account;
}

function reply(m, all) {
  const from = replyAccount();
  const mine = new Set(S.accounts.map((a) => a.email.toLowerCase()));
  const cc = all ? [...m.to, ...m.cc].filter((a) => !mine.has(a.email.toLowerCase()) && a.email !== m.from.email) : [];
  // Replying to your own message (from Sent) goes back to its recipients.
  const to = mine.has(m.from.email.toLowerCase()) ? m.to : [m.from];
  openCompose({ to: addrList(to), cc: addrList(cc), subject: prefixed("RE", m.subject), body: quoteBody(m), reply_to_message: m.id, account_id: from?.id, reply_account_id: S.viewAcct });
}

// ---- events ------------------------------------------------------------------

on("click", "compose", () => { S.drawer = false; layoutState(); openCompose({}); });
on("click", "sync", sync);
on("click", "rules", () => { S.drawer = false; layoutState(); openRules(null); });
on("click", "settings", () => { S.drawer = false; layoutState(); openSettings(); });
on("click", "add-account", () => { closeAccountMenu(); openSetup(true); });
on("click", "menu", () => { S.drawer = true; layoutState(); });
on("click", "scrim", () => closeDrawer());
export function closeDrawer() {
  S.drawer = false;
  layoutState();
}
on("click", "folder", (el) => openFolder(el.dataset.name));
on("dragover", "folderover", (el, e) => { e.preventDefault(); el.classList.add("drop"); });
on("dragleave", "folderleave", (el) => el.classList.remove("drop"));
on("drop", "folderdrop", (el, e) => {
  e.preventDefault();
  el.classList.remove("drop");
  const key = e.dataTransfer.getData("application/x-carteiro-thread");
  if (key) threadAct(key, "move-to:" + el.dataset.name);
});
on("dragstart", "threaddrag", (el, e) => e.dataTransfer.setData("application/x-carteiro-thread", el.dataset.key));
on("click", "thread", (el, e) => {
  if (e.target.closest("[data-kind], [data-click=thread-expand]")) return;
  // While picking, a tap adds or removes; Ctrl/Cmd-click picks on desktop.
  if (picking() || e.ctrlKey || e.metaKey) return togglePick(el.dataset.key);
  openThread(el.dataset.key);
});
on("click", "thread-expand", async (el, e) => {
  e.stopPropagation();
  const key = el.closest("[data-key]").dataset.key;
  S.openThreads[key] = !S.openThreads[key];
  renderList();
  if (S.openThreads[key] && !S.children[key]) {
    S.children[key] = (await guard(api.openThread(...splitRef(key)))) ?? [];
    renderList();
  }
});
on("click", "thread-msg", (el, e) => {
  if (picking() || e.ctrlKey || e.metaKey) return toggleMsgPick(el.dataset.key, +el.dataset.id);
  openThread(el.dataset.key, +el.dataset.id);
});
on("click", "thread-act", (el, e) => {
  e.stopPropagation();
  threadAct(el.closest("[data-key]").dataset.key, el.dataset.kind);
});
on("click", "back", () => { S.selected = null; S.messages = []; layoutState(); renderReader(); renderList(); });
on("click", "toggle-msg", (el, e) => {
  if (e.target.closest("button")) return;
  S.expanded[+el.dataset.id] = !S.expanded[+el.dataset.id];
  renderReader();
});
on("click", "msg-menu", (el) => { S.menuFor = S.menuFor === +el.dataset.id ? null : +el.dataset.id; renderReader(); });
on("click", "reply", (el) => reply(msgById(el), false));
on("click", "reply-all", (el) => reply(msgById(el), true));
on("click", "forward", (el) => {
  const m = msgById(el);
  openCompose({ account_id: replyAccount()?.id, subject: prefixed("FW", m.subject), body: `\n\n-----\nFrom: ${addrList([m.from])}\nSent: ${longDate(m.date)}\nTo: ${addrList(m.to)}\nSubject: ${m.subject}\n\n${m.body_text}\n${m.quoted_text}` });
});
on("click", "make-rule", (el) => { S.menuFor = null; openRules(ruleFromMessage(msgById(el))); });
on("click", "msg-delete", (el) => act("delete", [+el.dataset.id]));
on("click", "msg-unread", (el) => act("unread", [+el.dataset.id]));
const isCalendarFile = (a) => /\.(ics|ical|ifb|icalendar)$/i.test(a?.name ?? "") || /^(text\/calendar|application\/ics)$/i.test(a?.mime ?? "");

// Saving an attachment: it's checked by ClamAV first, which can take a moment,
// so the chip says so at once and further taps wait. Saved ones say where.
const saving = new Set(), saved = new Map();
on("click", "save-att", async (el) => {
  const id = +el.dataset.id, index = +el.dataset.index;
  const key = `${id}/${index}`;
  if (saving.has(key)) return;
  const a = S.messages.find((m) => m.id === id)?.attachments[index];
  if (isCalendarFile(a) && !(await calendarAttachment(id, index))) return;
  if (saved.has(key)) {
    const r = await choose(`Already saved to ${saved.get(key)}`, [["again", "Save another copy"]]);
    if (!r) return;
  }
  saving.add(key);
  setChip(key, "busy");
  try {
    const path = await api.saveAttachment(id, index);
    saved.set(key, path);
    toast(`Saved to ${path}`);
  } catch (e) {
    toast(String(e));
  } finally {
    saving.delete(key);
    setChip(key, saved.has(key) ? "done" : "");
  }
});
// Shows a chip's state (also after the conversation is redrawn).
function setChip(key, state) {
  const [id, index] = key.split("/");
  const chip = document.querySelector(`[data-click=save-att][data-id="${id}"][data-index="${index}"]`);
  if (!chip) return;
  chip.classList.toggle("busy", state === "busy");
  chip.classList.toggle("saved", state === "done");
  chip.setAttribute("aria-busy", state === "busy");
  const tag = chip.querySelector(".tag");
  if (tag) tag.textContent = state === "busy" ? "checking and saving…" : state === "done" ? "saved" : "scanned";
}

// A calendar file: offer to add it (or show it, if it's already there).
// Resolves true when the file should be saved to Downloads instead.
async function calendarAttachment(id, index) {
  let p;
  try { p = await api.previewIcs(id, index); } catch (e) { toast(String(e)); return false; }
  const n = p.events.length;
  const list = html`<ul class="ics-list">${p.events.slice(0, 5).map((e) => html`<li><strong>${e.title || "(no title)"}</strong>
    <span class="muted small">${eventWhen(e)}${e.recurring ? " · repeats" : ""}${e.location ? ` · ${e.location}` : ""}</span></li>`)}
    ${n > 5 ? html`<li class="muted small">and ${n - 5} more</li>` : ""}</ul>`;
  const cancel = p.method === "CANCEL";
  const question = cancel ? "This file cancels:" : p.in_calendar ? "Already in your Invitations calendar:" : n === 1 ? "Add this event to your calendar?" : `Add these ${n} events to your calendar?`;
  const options = [
    ...(p.in_calendar && !cancel ? [["show", "Show in calendar"]] : []),
    ["add", cancel ? "Remove it from my calendar" : p.in_calendar ? "Add again (restores removed dates)" : "Add to calendar"],
    ["save", "Save the file to Downloads"],
  ];
  const r = await choose(question, options.map(([k, l]) => [k, l, k === "add" && cancel]), "", () => null, list);
  if (!r) return false;
  if (r.choice === "save") return true;
  if (r.choice === "show") { closeDrawer(); await showEventDay(p.events[0]); return false; }
  try {
    await api.addIcs(id, index);
    toast(cancel ? "Removed from your calendar" : "Added to your Invitations calendar. Nobody is told.");
    if (!cancel) await showEventDay(p.events[0]);
  } catch (e) { toast(String(e)); }
  return false;
}
on("click", "plain", (el) => {
  const m = S.messages.find((x) => x.id === +el.dataset.id);
  const suspicious = (m?.warnings ?? []).some((w) => w.startsWith("A link"));
  S.plain[+el.dataset.id] = !(S.plain[+el.dataset.id] ?? suspicious);
  renderReader();
});
on("click", "quoted", (el) => { S.showQuoted[+el.dataset.id] = !S.showQuoted[+el.dataset.id]; renderReader(); });
on("click", "load-images", (el) => { S.images[+el.dataset.id] = true; renderReader(); });
on("click", "light", (el) => { S.light[+el.dataset.id] = !S.light[+el.dataset.id]; renderReader(); });
on("click", "conv", (el) => {
  if (!S.selected) return;
  const kind = el.dataset.kind;
  if (kind === "archive") return threadAct(S.selected, `move-to:${(S.viewFolders ?? S.folders).find((f) => f.special === "archive")?.name ?? "Archive"}`);
  // The open view updates at once; the server gets every copy.
  const field = { flag: ["flagged", true], unflag: ["flagged", false], read: ["seen", true], unread: ["seen", false] }[kind];
  if (field) { for (const m of S.messages) m[field[0]] = field[1]; renderReader(); }
  threadAct(S.selected, kind);
});
on("change", "move-to", (el) => el.value && S.selected && threadAct(S.selected, `move-to:${el.value}`));

let searchTimer;
on("input", "search", (el) => {
  S.query = el.value;
  for (const i of document.querySelectorAll("[data-input=search]")) if (i !== el) i.value = el.value;
  clearTimeout(searchTimer);
  searchTimer = setTimeout(refreshThreads, 150);
});

// Keyboard: j/k or arrows move through conversations, Delete removes.
document.addEventListener("keydown", (e) => {
  if (e.target.closest("input, textarea, select, [contenteditable]") || $("#modal").childElementCount || $("#compose").childElementCount) return;
  const i = S.threads.findIndex((t) => t.ref === S.selected);
  if ((e.key === "ArrowDown" || e.key === "j") && S.threads.length) { e.preventDefault(); openThread(S.threads[Math.min(i + 1, S.threads.length - 1)].ref); }
  if ((e.key === "ArrowUp" || e.key === "k") && S.threads.length) { e.preventDefault(); openThread(S.threads[Math.max(i - 1, 0)].ref); }
  if (e.key === "Delete" && i >= 0) threadAct(S.threads[i].ref, "delete");
  if (e.key === "n" && !e.ctrlKey && !e.metaKey) openCompose({});
});

// Pull to refresh on touch screens.
let pullStart = -1;
const threadsEl = $("#threads");
threadsEl.addEventListener("touchstart", (e) => (pullStart = threadsEl.scrollTop === 0 ? e.touches[0].clientY : -1), { passive: true });
threadsEl.addEventListener("touchmove", (e) => {
  if (pullStart >= 0) threadsEl.style.paddingTop = Math.max(0, Math.min(80, e.touches[0].clientY - pullStart)) + "px";
}, { passive: true });
threadsEl.addEventListener("touchend", () => {
  if (parseInt(threadsEl.style.paddingTop) > 60) sync();
  threadsEl.style.paddingTop = "";
  pullStart = -1;
});

// Background sync (tray mode) finished: refresh what's on screen.
// ---- older mail as you scroll -----------------------------------------------------

// Reaching the end of the list shows more: first the next page of what's
// on this device, then older messages listed from the server.
const older = { busy: false, done: new Set() };
async function loadMore() {
  if (older.busy || !S.account || S.folder === UNIFIED || S.query || S.filter) return;
  const where = `${S.account.id}/${S.folder}`;
  if (older.done.has(where)) return;
  older.busy = true;
  renderOlderRow(true);
  try {
    let more = (await guard(api.threads(S.account.id, S.folder, null, S.threads.length, 100))) ?? [];
    if (!more.length && (await guard(api.loadOlder(S.account.id, S.folder))) > 0) {
      more = (await guard(api.threads(S.account.id, S.folder, null, S.threads.length, 100))) ?? [];
    }
    if (!more.length) older.done.add(where);
    const seen = new Set(S.threads.map((t) => t.ref));
    S.threads.push(...more.map((t) => ({ ...t, acct: S.account.id, folder: S.folder, ref: refOf(S.account.id, t.thread_key) })).filter((t) => !seen.has(t.ref)));
  } finally {
    older.busy = false;
  }
  renderList();
}
function renderOlderRow(loading) {
  let row = $("#threads .older-row");
  if (!loading) return row?.remove();
  if (!row) { row = document.createElement("div"); row.className = "older-row muted small"; $("#threads").append(row); }
  row.textContent = "Loading older mail…";
}
$("#threads").addEventListener("scroll", (e) => {
  const el = e.currentTarget;
  if (el.scrollTop + el.clientHeight > el.scrollHeight - 300) loadMore();
}, { passive: true });

// ---- offline download progress ---------------------------------------------------

listen("offline-progress", (p) => {
  const bar = $("#progress");
  bar.hidden = false;
  const pct = p.total ? Math.round((p.done / p.total) * 100) : 0;
  const what = p.phase === "listing" ? "Finding older mail" : "Downloading mail for offline use";
  put(bar, html`<div class="progress-text"><strong>${what}</strong> · ${p.folder} · ${p.account}
      <span class="muted">${p.done.toLocaleString()} of ${p.total.toLocaleString()}</span></div>
    <div class="progress-track" role="progressbar" aria-valuenow="${pct}" aria-valuemin="0" aria-valuemax="100"><div style="width:${pct}%"></div></div>`);
});
listen("offline-done", (r) => {
  $("#progress").hidden = true;
  if (r?.error) toast(`Offline download stopped: ${r.error}`);
  else if (r?.report && (r.report.downloaded || r.report.listed)) toast(`Offline copy updated: ${r.report.downloaded.toLocaleString()} downloaded, ${r.report.listed.toLocaleString()} older messages listed`);
  refreshThreads();
});

// Invitations arriving by mail show up in an open calendar right away.
listen("calendar-changed", () => refreshCalendar());
listen("mail-synced", () => { refreshFolders(); refreshThreads(); syncPhoneContacts(); });
// Android: the shell asks the page to open the browser (sign-in).
listen("open-url", (url) => guard(api.openLink(url)));
// Messages opened while the virus scanner was still starting showed plain
// text only; open them again now that they can be scanned.
listen("scanner-ready", () => {
  if (S.selected && S.messages.some((m) => m.verdict.status === "not_scanned")) openThread(S.selected);
});

// Google / Microsoft sign-in needs the app registration a build was made
// with; without one, those buttons aren't offered at all.
api.getSettings().then((cfg) => {
  document.body.classList.toggle("no-google", !cfg.google?.client_id);
  document.body.classList.toggle("no-microsoft", !cfg.microsoft?.client_id);
}).catch(() => {});
loadAccounts();
// Working hours are local time; the engine has no time zone database.
api.setUtcOffset().catch(() => {});
setInterval(() => api.setUtcOffset().catch(() => {}), 30 * 60 * 1000);
syncPhoneContacts();
