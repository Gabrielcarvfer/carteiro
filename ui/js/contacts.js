// Contacts: your own (created or imported here), people you mail with
// (collected automatically), the phone's (Android), and synced address books
// (Google, Microsoft, CardDAV). A list beside the details on desktop; list,
// then details, on phones.

import { api } from "./api.js";
import { $, ask, html, on, put } from "./dom.js";
import { avatar, icon, shortDate } from "./format.js";
import { activityButton, showMessage, syncLayer, toast } from "./app.js";
import { openCompose } from "./dialogs.js";

const K = { query: "", list: [], sources: [], selected: null, jumper: false, threads: null };

/** Leaving the section closes the open contact. */
export function leaveContacts() {
  if (K.selected || K.jumper) { K.selected = null; K.jumper = false; render(); }
}

export async function showContacts() {
  await load();
  sync();
}

async function load() {
  [K.list, K.sources] = await Promise.all([api.listContacts(K.query).catch(() => []), api.contactSources().catch(() => [])]);
  render();
}

async function sync() {
  await syncPhoneContacts();
  const errors = await api.syncContacts().catch((e) => [String(e)]);
  if (errors?.length) toast(errors[0]);
  await load();
}

// ---- the phone's contacts (Android) ---------------------------------------------

const PHONE = window.CarteiroAndroid;
const PHONE_KEY = "carteiro.phoneContacts";
const phoneWanted = () => { try { return localStorage.getItem(PHONE_KEY) === "1"; } catch { return false; } };

/** Mirrors the phone's address book when the user asked for it. */
export async function syncPhoneContacts() {
  if (!PHONE?.readContacts || !phoneWanted() || !PHONE.contactsAllowed()) return;
  try {
    const list = JSON.parse(PHONE.readContacts());
    await api.syncPhoneContacts(list.map(([id, c]) => [id, { uid: id, notes: "", ...c }]));
  } catch (e) { toast(`Phone contacts: ${e}`); }
}

async function startPhoneContacts() {
  try { localStorage.setItem(PHONE_KEY, "1"); } catch {}
  if (!PHONE.contactsAllowed()) {
    // Android shows its own question; the answer comes back here.
    const granted = await new Promise((resolve) => { window.carteiroContactsPermission = resolve; PHONE.askContacts(); });
    if (!granted) {
      try { localStorage.removeItem(PHONE_KEY); } catch {}
      return toast("Carteiro wasn't allowed to read the phone's contacts");
    }
  }
  await syncPhoneContacts();
  await load();
  books();
}

const sourceName = (c) => K.sources.find((s) => s.id === c.source_id)?.name ?? "";

function render() {
  const groups = [];
  for (const c of K.list) {
    const letter = letterOf(c.name);
    const g = groups[groups.length - 1];
    if (g && g[0] === letter) g[1].push(c);
    else groups.push([letter, [c]]);
  }
  const sel = K.list.find((c) => c.id === K.selected);
  put("#contacts", html`
    <div class="ct-list">
      <header class="ct-bar">
        <h2>Contacts</h2>
        <span class="spacer"></span>
        ${activityButton()}
        <button class="icon-btn" title="Sync address books" data-click="ct-sync">${icon("sync")}</button>
        <button class="icon-btn" title="Address books" data-click="ct-books">${icon("calendar")}</button>
        <button class="btn" data-click="ct-import">Import</button>
        <button class="btn" data-click="ct-export">Export</button>
        <button class="btn primary" data-click="ct-new">${icon("plus", 15)} New</button>
        <input type="file" name="ct-file" accept=".vcf,.vcard,.csv,.ldif,.ldi,text/vcard,text/csv" hidden data-change="ct-file" />
      </header>
      <input class="ct-search" type="search" placeholder="Search contacts" aria-label="Search contacts" data-input="ct-search" value="${K.query}" />
      <div class="ct-items">
        ${K.list.length ? groups.map(([l, cs]) => html`<button class="ct-letter" data-click="ct-jumper" data-letter="${l}" aria-label="Jump to a letter">${l}</button>${cs.map((c) => html`
          <button class="ct-item ${c.id === K.selected ? "on" : ""}" data-click="ct-open" data-id="${c.id}">
            ${avatar(c.name, 34)}<span class="who"><strong>${c.name}</strong><span class="muted small">${c.emails[0] ?? c.phones[0] ?? ""}</span></span>
          </button>`)}`)
        : html`<p class="muted empty">${K.query ? `No contacts match "${K.query}".` : html`No contacts yet. <button class="linkish" data-click="ct-import">Import</button> a vCard, CSV or LDIF file from Gmail, Outlook or Thunderbird, or add an address book.`}</p>`}
      </div>
      ${K.jumper ? jumper(new Set(groups.map(([l]) => l))) : ""}
    </div>
    <div class="ct-detail">${sel ? detail(sel) : html`<div class="placeholder muted">${icon("person", 48)}<p>Select a contact</p></div>`}</div>`);
  $("#contacts").classList.toggle("has-sel", !!sel);
  syncLayer("contact", !!sel);
  syncLayer("jumper", K.jumper);
}

// Groups by the first letter without accents (Á under A); digits and symbols under #.
const LETTERS = ["#", ..."ABCDEFGHIJKLMNOPQRSTUVWXYZ"];
function letterOf(name) {
  const l = (name.normalize("NFD")[0] ?? "").toLocaleUpperCase();
  return LETTERS.includes(l) ? l : "#";
}

// Tapping a heading shows every letter; the ones with contacts jump there.
function jumper(present) {
  return html`<div class="ct-jumper" data-click="ct-jumper-close">
    ${LETTERS.map((l) => present.has(l)
      ? html`<button class="on" data-click="ct-jump" data-letter="${l}">${l}</button>`
      : html`<button disabled>${l}</button>`)}
  </div>`;
}
document.addEventListener("jumper-back", () => { K.jumper = false; render(); });
on("click", "ct-jumper", () => { K.jumper = true; render(); });
on("click", "ct-jumper-close", () => { K.jumper = false; render(); });
on("click", "ct-jump", (el, e) => {
  e.stopPropagation();
  K.jumper = false;
  render();
  $(`#contacts .ct-letter[data-letter="${el.dataset.letter}"]`)?.scrollIntoView({ block: "start" });
});
document.addEventListener("contact-back", () => { K.selected = null; render(); });

function detail(c) {
  return html`<div class="pane">
    <header class="toolbar">
      <button class="icon-btn show-narrow" aria-label="Back" data-click="ct-back">${icon("back", 22)}</button>
      <span class="spacer"></span>
      ${c.read_only ? "" : html`<button class="btn" data-click="ct-edit">Edit</button>`}
      <button class="btn danger" data-click="ct-delete">Delete</button>
    </header>
    <div class="ct-card">
      ${avatar(c.name, 64)}
      <h1>${c.name}</h1>
      ${c.organization ? html`<p class="muted">${c.organization}</p>` : ""}
      ${c.emails.map((e) => html`<p><button class="linkish" data-click="ct-mail" data-to="${e}">${icon("compose", 14)} ${e}</button></p>`)}
      ${c.phones.map((p) => html`<p>${p}</p>`)}
      ${c.notes ? html`<pre class="text">${c.notes}</pre>` : ""}
      <p class="muted small">${origin(c)}</p>
    </div>
    ${c.emails.length ? html`<section class="ct-threads">
      <h3>Conversations</h3>
      ${K.threads?.id !== c.id ? html`<p class="muted small">Loading…</p>`
        : !K.threads.list.length ? html`<p class="muted small">No mail with ${c.name} yet.</p>`
        : K.threads.list.map((t) => html`<button class="ct-thread ${t.unread ? "unread" : ""}" data-click="ct-thread" data-acct="${t.account_id}" data-key="${t.thread_key}" data-id="${t.latest_id}">
          <span class="row"><strong class="ellipsis">${t.subject || "(no subject)"}</strong>${t.count > 1 ? html`<span class="count">${t.count}</span>` : ""}<span class="muted small date">${shortDate(t.last_date)}</span></span>
          <span class="muted small ellipsis">${t.snippet}</span></button>`)}
    </section>` : ""}
  </div>`;
}

// The open contact's conversations, loaded when it's opened.
async function loadThreads(c) {
  K.threads = null;
  if (!c?.emails.length) return;
  const list = (await api.contactThreads(c.emails).catch(() => [])) ?? [];
  if (K.selected !== c.id) return;
  K.threads = { id: c.id, list };
  render();
}
on("click", "ct-thread", (el) => showMessage(+el.dataset.acct, el.dataset.key, +el.dataset.id));

function origin(c) {
  const src = K.sources.find((s) => s.id === c.source_id);
  if (src?.kind === "collected") return "Added from your mail";
  if (src?.kind === "android") return "From the phone's contacts (change it there)";
  return c.read_only ? `From ${sourceName(c)} (synced: change it there)` : "On this device";
}

function editor(c) {
  put("#modal", html`
    <div class="modal-backdrop" data-click="modal-backdrop">
      <div class="modal" role="dialog" aria-label="Contact">
        <header>${c.id ? "Edit contact" : "New contact"}<button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button></header>
        <div class="content form">
          <label class="stack"><span>Name</span><input name="name" value="${c.name ?? ""}" /></label>
          <label class="stack"><span>Email addresses (one per line)</span><textarea name="emails" rows="2">${(c.emails ?? []).join("\n")}</textarea></label>
          <label class="stack"><span>Phone numbers (one per line)</span><textarea name="phones" rows="2">${(c.phones ?? []).join("\n")}</textarea></label>
          <label class="stack"><span>Organization</span><input name="organization" value="${c.organization ?? ""}" /></label>
          <label class="stack"><span>Notes</span><textarea name="notes" rows="3">${c.notes ?? ""}</textarea></label>
          <div class="error" hidden></div>
        </div>
        <footer><span class="spacer"></span><button class="btn" data-click="modal-close">Cancel</button><button class="btn primary" data-click="ct-save" data-id="${c.id ?? ""}">Save</button></footer>
      </div>
    </div>`);
  $("#modal [name=name]").focus();
}

function books() {
  put("#modal", html`
    <div class="modal-backdrop" data-click="modal-backdrop">
      <div class="modal" role="dialog" aria-label="Address books">
        <header>Address books<button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button></header>
        <div class="content form">
          ${K.sources.map((s) => html`<div class="rule"><div class="rule-main"><div class="rule-name">${s.name}</div>
            <div class="muted small">${{ local: "On this device", collected: "People you mail with, added automatically", android: "The phone's contacts", carddav: "CardDAV", google: "Google", microsoft: "Microsoft 365 / Outlook" }[s.kind] ?? s.kind}${s.url ? ` · ${s.url}` : ""}</div></div>
            ${s.kind === "carddav" || s.kind === "android" ? html`<button class="btn danger" data-click="ct-book-remove" data-id="${s.id}">${s.kind === "android" ? "Stop" : "Remove"}</button>` : ""}</div>`)}
          ${PHONE?.readContacts && !K.sources.some((s) => s.kind === "android") ? html`<div class="rule"><div class="rule-main"><div class="rule-name">Phone contacts</div>
            <div class="muted small">Bring in the contacts saved on this phone, and keep them up to date.</div></div>
            <button class="btn primary" data-click="ct-phone">Bring in</button></div>` : ""}
          <p class="muted small">Google and Microsoft accounts bring their contacts automatically. Add any other CardDAV address book (Fastmail, iCloud, Nextcloud, mailbox.org…):</p>
          <label class="stack"><span>Server address</span><input name="url" placeholder="https://carddav.example.com/" autocapitalize="off" /></label>
          <label class="stack"><span>User name</span><input name="username" autocapitalize="off" /></label>
          <label class="stack"><span>Password (an app password where the service offers one)</span><input name="password" type="password" /></label>
          <label class="stack"><span>Name</span><input name="bookname" placeholder="Work contacts" /></label>
          <div class="error" hidden></div>
        </div>
        <footer><span class="spacer"></span><button class="btn primary" data-click="ct-book-add">Add address book</button></footer>
      </div>
    </div>`);
}

const lines = (v) => v.split(/[\n,;]+/).map((s) => s.trim()).filter(Boolean);
const fail = (msg) => { const e = $("#modal .error"); e.textContent = String(msg); e.hidden = false; };

let searchTimer;
on("input", "ct-search", (el) => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(async () => {
    K.query = el.value.trim();
    K.list = await api.listContacts(K.query).catch(() => []);
    // Redraw the list only, so the search box keeps focus.
    const pos = el.selectionStart;
    render();
    const box = $("#contacts .ct-search");
    box.focus();
    box.setSelectionRange(pos, pos);
  }, 150);
});
on("click", "ct-open", (el) => { K.selected = +el.dataset.id; render(); loadThreads(K.list.find((c) => c.id === K.selected)); });
on("click", "ct-back", () => { K.selected = null; render(); });
on("click", "ct-new", () => editor({}));
on("click", "ct-edit", () => editor(K.list.find((c) => c.id === K.selected)));
on("click", "ct-mail", (el) => openCompose({ to: el.dataset.to }));
on("click", "ct-sync", () => sync());
on("click", "ct-books", () => books());
on("click", "ct-save", async (el) => {
  const v = (n) => $(`#modal [name=${n}]`).value;
  try {
    const old = K.list.find((c) => c.id === +el.dataset.id);
    const id = await api.saveContact(old ? old.id : null, { uid: old?.uid ?? "", name: v("name"), emails: lines(v("emails")), phones: v("phones").split("\n").map((s) => s.trim()).filter(Boolean), organization: v("organization"), notes: v("notes") });
    put("#modal", "");
    K.selected = id;
    await load();
    loadThreads(K.list.find((c) => c.id === id));
  } catch (e) { fail(e); }
});
on("click", "ct-delete", async () => {
  const c = K.list.find((x) => x.id === K.selected);
  if (!c) return;
  const note = c.read_only ? ` It stays in ${sourceName(c)}, but won't come back here.` : "";
  if (!(await ask(`Delete ${c.name}?${note}`))) return;
  try { await api.deleteContact(c.id); K.selected = null; await load(); } catch (e) { toast(String(e)); }
});
on("click", "ct-import", () => $("#contacts [name=ct-file]").click());
on("change", "ct-file", async (el) => {
  const file = el.files?.[0];
  el.value = "";
  if (!file) return;
  try {
    const r = await api.importContacts(await file.text());
    toast(`Imported ${file.name}: ${r.added} new, ${r.merged} merged into existing contacts`);
    await load();
  } catch (e) { toast(String(e)); }
});
on("click", "ct-export", async () => {
  try { toast(`Contacts saved to ${await api.exportContacts()}`); } catch (e) { toast(String(e)); }
});
on("click", "ct-book-add", async (el) => {
  const v = (n) => $(`#modal [name=${n}]`).value.trim();
  el.disabled = true;
  el.textContent = "Checking…";
  try {
    await api.addCarddav(v("url"), v("username"), $("#modal [name=password]").value, v("bookname"));
    put("#modal", "");
    await load();
  } catch (e) {
    fail(e);
    el.disabled = false;
    el.textContent = "Add address book";
  }
});
on("click", "ct-phone", () => startPhoneContacts());
on("click", "ct-book-remove", async (el) => {
  if (K.sources.find((s) => s.id === +el.dataset.id)?.kind === "android") {
    try { localStorage.removeItem(PHONE_KEY); } catch {}
  }
  await api.removeContactSource(+el.dataset.id).catch((e) => toast(String(e)));
  K.sources = await api.contactSources();
  books();
  load();
});
