// Compose window and modal dialogs: rules, account setup, settings.

import { api } from "./api.js";
import { $, ask, html, on, put } from "./dom.js";
import { bytes, folderLabel, icon } from "./format.js";
import { WEEK_START_KEY, refreshCalendar, weekStart } from "./calendar.js";
import { S, SWIPE_ACTIONS, UNIFIED, accountById, loadAccounts, refreshFolders, refreshThreads, selectAccount, swipeAction, toast } from "./app.js";

const closeModal = () => put("#modal", "");
const modal = (title, body, footer = "", closable = true) => html`
  <div class="modal-backdrop" data-click="${closable ? "modal-backdrop" : ""}">
    <div class="modal" role="dialog" aria-label="${title}">
      <header>${title}${closable ? html`<button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button>` : ""}</header>
      <div class="content">${body}</div>
      ${footer ? html`<footer>${footer}</footer>` : ""}
    </div>
  </div>`;

on("click", "modal-close", closeModal);
on("click", "modal-backdrop", (el, e) => e.target === el && closeModal());
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if ($("#compose").childElementCount) put("#compose", "");
  else if ($("#modal .icon-btn[data-click=modal-close]")) closeModal();
});

// ---- compose -----------------------------------------------------------------

let replyTo = null;
let replyAccount = null;

export function openCompose(init) {
  replyTo = init.reply_to_message ?? null;
  // The replied-to message lives in this account's store (threading headers).
  replyAccount = init.reply_account_id ?? null;
  const from = init.account_id ?? S.account?.id;
  const showCc = !!init.cc;
  put("#compose", html`
    <div class="compose" role="dialog" aria-label="New message">
      <header>
        <button class="icon-btn" aria-label="Discard" data-click="compose-close">${icon("close")}</button>
        <span class="title">${replyTo ? "Reply" : "New message"}</span>
        <button class="btn primary" data-click="send">${icon("sent", 15)} Send</button>
      </header>
      <div class="fields">
        ${S.accounts.length > 1 ? html`<label><span>From</span><select name="from">
          ${S.accounts.map((a) => html`<option value="${a.id}" ${a.id === from ? "selected" : ""}>${a.name} &lt;${a.email}&gt;</option>`)}
        </select></label>` : ""}
        <label><span>To</span><input name="to" value="${init.to ?? ""}" autocomplete="off" list="addr-suggest" data-input="addr-suggest" />
          ${showCc ? "" : html`<button class="linkish" data-click="show-cc">Cc/Bcc</button>`}</label>
        <label class="cc" ${showCc ? "" : "hidden"}><span>Cc</span><input name="cc" value="${init.cc ?? ""}" autocomplete="off" list="addr-suggest" data-input="addr-suggest" /></label>
        <label class="cc" ${showCc ? "" : "hidden"}><span>Bcc</span><input name="bcc" autocomplete="off" list="addr-suggest" data-input="addr-suggest" /></label>
        <label><span>Subject</span><input name="subject" value="${init.subject ?? ""}" /></label>
      </div>
      <datalist id="addr-suggest"></datalist>
      <textarea name="body" placeholder="Write your message">${init.body ?? ""}</textarea>
      <div class="error" hidden></div>
    </div>`);
  const ta = $("#compose textarea");
  if (replyTo) { ta.focus(); ta.setSelectionRange(0, 0); } else $("#compose [name=to]").focus();
}

on("click", "compose-close", () => put("#compose", ""));

// Address fields hold a comma-separated list; suggestions complete the last
// entry and keep the ones before it.
let suggestSeq = 0;
on("input", "addr-suggest", async (el) => {
  const parts = el.value.split(",");
  const last = parts.pop().trim();
  const seq = ++suggestSeq;
  const found = last.length < 2 || last.includes("@") && last.includes(".") ? [] : await api.suggestAddresses(last).catch(() => []);
  if (seq !== suggestSeq) return;
  const before = parts.map((p) => p.trim()).filter(Boolean).map((p) => p + ", ").join("");
  put("#addr-suggest", html`${found.map(([name, email]) => html`<option value="${before}${name && name !== email ? `${name} <${email}>` : email}">`)}`);
});
on("click", "show-cc", (el) => { for (const l of document.querySelectorAll("#compose .cc")) l.hidden = false; el.remove(); });
on("click", "send", send);
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && $("#compose").childElementCount) send();
});

async function send() {
  const f = (n) => $(`#compose [name=${n}]`).value;
  const btn = $("#compose [data-click=send]");
  const err = $("#compose .error");
  btn.disabled = true;
  err.hidden = true;
  try {
    const fromEl = $("#compose [name=from]");
    const acct = fromEl ? +fromEl.value : S.account.id;
    // Threading headers come from the store of the account holding the parent.
    const parent = replyTo != null && (replyAccount == null || replyAccount === acct) ? replyTo : null;
    const result = await api.send(acct, { to: f("to"), cc: f("cc"), bcc: f("bcc"), subject: f("subject"), body: f("body"), reply_to_message: parent });
    put("#compose", "");
    toast(result === "queued" ? "You're offline: saved to Outbox, will send automatically" : "Sent");
    refreshThreads();
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
    btn.disabled = false;
  }
}

// ---- rules ---------------------------------------------------------------------

const FIELDS = [["from", "From"], ["to", "To"], ["cc", "Cc"], ["any_recipient", "To or Cc"], ["subject", "Subject"],
  ["body", "Message body"], ["subject_or_body", "Subject or body"], ["has_attachment", "Has attachment"]];
const OPS = [["contains", "includes"], ["not_contains", "doesn't include"], ["is", "is exactly"],
  ["starts_with", "starts with"], ["ends_with", "ends with"], ["matches", "matches pattern"]];
const ACTIONS = [["move_to", "Move to folder"], ["copy_to", "Copy to folder"], ["mark_read", "Mark as read"], ["flag", "Flag it"], ["delete", "Delete"]];

let rules = [];
let editing = null;
let ruleError = null;
let rulesAccount = null; // account whose rules are shown

const visibleRules = () => rules.filter((r) => r.account_id == null || r.account_id === rulesAccount);

const options = (list, cur) => list.map(([v, l]) => html`<option value="${v}" ${v === cur ? "selected" : ""}>${l}</option>`);
const newCond = () => ({ field: "subject", op: "contains", value: "" });
const newAction = (type) => (type === "move_to" || type === "copy_to" ? { type, folder: "" } : { type });
const blankRule = () => ({ id: null, name: "", enabled: true, account_id: rulesAccount, match_all: true,
  conditions: [{ field: "from", op: "contains", value: "" }], exceptions: [], actions: [{ type: "move_to", folder: "" }], stop_processing: true });

export const ruleFromMessage = (m) => ({ ...blankRule(), name: `From ${m.from.name || m.from.email}`,
  conditions: [{ field: "from", op: "contains", value: m.from.email }] });

export async function openRules(seed) {
  rulesAccount = S.viewAcct ?? S.account?.id ?? null;
  editing = seed ? { ...structuredClone(seed), account_id: seed.account_id ?? rulesAccount } : null;
  ruleError = null;
  rules = (await api.rules()) ?? [];
  renderRules();
}

const label = (list, v) => list.find((x) => x[0] === v)?.[1] ?? v;
const describe = (r) =>
  (r.conditions.length
    ? r.conditions.map((c) => c.field === "has_attachment" ? "Has attachment" : `${label(FIELDS, c.field)} ${label(OPS, c.op)} "${c.value}"`).join(r.match_all ? " and " : " or ")
    : "All messages") + " → " +
  r.actions.map((a) => (a.folder !== undefined ? `${a.type === "move_to" ? "move to" : "copy to"} ${a.folder}` : label(ACTIONS, a.type).toLowerCase())).join(", ");

const condRows = (list, key) => list.map((c, i) => html`
  <div class="row">
    <select data-change="rule-set" data-path="${key}.${i}.field">${options(FIELDS, c.field)}</select>
    ${c.field === "has_attachment" ? "" : html`
      <select data-change="rule-set" data-path="${key}.${i}.op">${options(OPS, c.op)}</select>
      <input data-input="rule-set" data-path="${key}.${i}.value" value="${c.value}" placeholder="${c.op === "matches" ? "pattern: * and ? wildcards" : "text; separate several with ;"}" />`}
    <button class="icon-btn" aria-label="Remove" data-click="rule-remove" data-path="${key}.${i}">${icon("close", 14)}</button>
  </div>`);

function renderRules() {
  if (!editing) {
    const list = visibleRules();
    put("#modal", modal("Rules", html`
      ${S.accounts.length > 1 ? html`<label class="stack narrow-field"><span>Account</span>
        <select data-change="rules-account">${S.accounts.map((a) => html`<option value="${a.id}" ${a.id === rulesAccount ? "selected" : ""}>${a.email}</option>`)}</select></label>` : ""}
      <p class="muted intro">Rules run on new mail in this account's Inbox, top to bottom.</p>
      ${list.length ? list.map((r, i) => html`
        <div class="rule ${r.enabled ? "" : "off"}">
          <input type="checkbox" ${r.enabled ? "checked" : ""} data-change="rule-toggle" data-i="${i}" aria-label="Enabled" />
          <div class="rule-main"><div class="rule-name">${r.name}${r.account_id == null && S.accounts.length > 1 ? html` <span class="folder-tag">all accounts</span>` : ""}</div><div class="muted small">${describe(r)}</div></div>
          <button class="icon-btn" aria-label="Move up" data-click="rule-move" data-i="${i}" data-d="-1">${icon("up", 15)}</button>
          <button class="icon-btn" aria-label="Move down" data-click="rule-move" data-i="${i}" data-d="1">${icon("down", 15)}</button>
          <button class="btn" data-click="rule-edit" data-i="${i}">Edit</button>
          <button class="btn danger" data-click="rule-delete" data-i="${i}">Delete</button>
        </div>`) : html`<p class="muted">No rules yet. Create one, or use "Create rule…" on any message.</p>`}
      ${ruleError ? html`<div class="error">${ruleError}</div>` : ""}`,
      html`<button class="btn" data-click="rules-run" ${list.length ? "" : "disabled"}>Run rules on this folder now</button>
        <button class="btn primary" data-click="rule-new">+ Add new rule</button>`));
    return;
  }
  const r = editing;
  put("#modal", modal(r.id ? "Edit rule" : "New rule", html`
    <div class="form">
      <label class="stack"><span>Name</span><input data-input="rule-set" data-path="name" value="${r.name}" placeholder="e.g. Newsletters" /></label>
      <section>
        <div class="sect-head"><strong>When a message arrives that matches</strong>
          <select data-change="rule-set" data-path="match_all" data-bool="1">
            <option value="1" ${r.match_all ? "selected" : ""}>all of these</option>
            <option value="0" ${r.match_all ? "" : "selected"}>any of these</option>
          </select></div>
        ${condRows(r.conditions, "conditions")}
        <button class="linkish" data-click="rule-add" data-key="conditions">+ Add a condition</button>
      </section>
      <section>
        <strong>Do the following</strong>
        ${r.actions.map((a, i) => html`
          <div class="row">
            <select data-change="rule-action" data-i="${i}">${options(ACTIONS, a.type)}</select>
            ${a.folder !== undefined ? html`<input list="folder-names" data-input="rule-set" data-path="actions.${i}.folder" value="${a.folder}" placeholder="Folder (new folders are created)" />` : ""}
            <button class="icon-btn" aria-label="Remove" data-click="rule-remove" data-path="actions.${i}">${icon("close", 14)}</button>
          </div>`)}
        <button class="linkish" data-click="rule-add" data-key="actions">+ Add an action</button>
      </section>
      <section>
        <strong>Except if</strong>
        ${condRows(r.exceptions, "exceptions")}
        <button class="linkish" data-click="rule-add" data-key="exceptions">+ Add an exception</button>
      </section>
      <label class="check"><input type="checkbox" data-change="rule-set" data-path="stop_processing" data-check="1" ${r.stop_processing ? "checked" : ""} /> Stop processing more rules</label>
      ${S.accounts.length > 1 ? html`<label class="stack"><span>Applies to</span>
        <select data-change="rule-set" data-path="account_id" data-num="1">
          <option value="" ${r.account_id == null ? "selected" : ""}>All accounts</option>
          ${S.accounts.map((a) => html`<option value="${a.id}" ${a.id === r.account_id ? "selected" : ""}>${a.email}</option>`)}
        </select></label>` : ""}
      <datalist id="folder-names">${S.folders.map((f) => html`<option value="${f.name}">${folderLabel(f)}</option>`)}</datalist>
      ${ruleError ? html`<div class="error">${ruleError}</div>` : ""}
    </div>`,
    html`<button class="btn" data-click="rule-cancel">Cancel</button><button class="btn primary" data-click="rule-save">Save</button>`));
}

function setPath(obj, path, value) {
  const keys = path.split(".");
  const last = keys.pop();
  for (const k of keys) obj = obj[k];
  obj[last] = value;
}
function ruleSet(el) {
  let v = el.value;
  if (el.dataset.check) v = el.checked;
  else if (el.dataset.bool) v = v === "1";
  else if (el.dataset.num) v = v === "" ? null : +v;
  setPath(editing, el.dataset.path, v);
}
on("input", "rule-set", ruleSet);
on("change", "rule-set", (el) => {
  ruleSet(el);
  if (el.tagName === "SELECT") renderRules(); // field/op changes alter the row
});
on("change", "rule-action", (el) => { editing.actions[+el.dataset.i] = newAction(el.value); renderRules(); });
on("click", "rule-add", (el) => {
  const key = el.dataset.key;
  editing[key].push(key === "actions" ? newAction("mark_read") : newCond());
  renderRules();
});
on("click", "rule-remove", (el) => {
  const [key, i] = el.dataset.path.split(".");
  editing[key].splice(+i, 1);
  renderRules();
});
on("click", "rule-new", () => { editing = blankRule(); ruleError = null; renderRules(); });
on("click", "rule-edit", (el) => { editing = structuredClone(visibleRules()[+el.dataset.i]); ruleError = null; renderRules(); });
on("click", "rule-cancel", () => { editing = null; ruleError = null; renderRules(); });
on("click", "rule-save", async () => {
  try {
    await api.saveRule(editing);
    editing = null;
    ruleError = null;
    rules = await api.rules();
  } catch (e) { ruleError = String(e); }
  renderRules();
});
on("change", "rule-toggle", async (el) => {
  await api.saveRule({ ...visibleRules()[+el.dataset.i], enabled: el.checked });
  rules = await api.rules();
  renderRules();
});
on("click", "rule-delete", async (el) => {
  await api.deleteRule(visibleRules()[+el.dataset.i].id);
  rules = await api.rules();
  renderRules();
});
on("click", "rule-move", async (el) => {
  const list = visibleRules();
  const i = +el.dataset.i, j = i + +el.dataset.d;
  if (j < 0 || j >= list.length) return;
  // Swap the two visible rules' positions within the full ordering.
  const ids = rules.map((r) => r.id);
  const a = ids.indexOf(list[i].id), b = ids.indexOf(list[j].id);
  [ids[a], ids[b]] = [ids[b], ids[a]];
  await api.reorderRules(ids);
  rules = await api.rules();
  renderRules();
});
on("change", "rules-account", (el) => { rulesAccount = +el.value; renderRules(); });
on("click", "rules-run", async () => {
  try {
    const folder = rulesAccount === S.account?.id && S.folder !== UNIFIED ? S.folder : "INBOX";
    const n = await api.runRules(rulesAccount, folder);
    toast(`${n} message(s) updated`);
    refreshFolders();
    refreshThreads();
  } catch (e) { ruleError = String(e); renderRules(); }
});

// ---- account setup ---------------------------------------------------------------

const PORTS = { imap: { tls: 993, starttls: 143, none: 143 }, pop3: { tls: 995, starttls: 110, none: 110 }, smtp: { tls: 465, starttls: 587, none: 25 } };
let setup = null;

export function openSetup(canCancel) {
  setup = { canCancel, step: "email", email: "", name: "", username: "", protocol: "imap",
    incoming: { host: "", port: 993, security: "tls", allow_insecure: false },
    smtp: { host: "", port: 465, security: "tls", allow_insecure: false }, note: null, error: null };
  renderSetup();
}

const serverBlock = (title, ep, kind) => html`
  <fieldset>
    <legend>${title}</legend>
    <div class="grid">
      <label class="host"><span>Server</span><input name="${kind}-host" value="${ep.host}" autocapitalize="off" spellcheck="false" /></label>
      <label><span>Port</span><input name="${kind}-port" type="number" min="1" max="65535" value="${ep.port}" /></label>
      <label><span>Encryption</span>
        <select data-change="setup-security" data-kind="${kind}">
          ${options([["tls", "SSL/TLS"], ["starttls", "STARTTLS"], ["none", "None (insecure)"]], ep.security)}
        </select></label>
    </div>
    ${ep.security === "none" ? html`<label class="warn"><input type="checkbox" name="${kind}-insecure" ${ep.allow_insecure ? "checked" : ""} />
      I understand my password and mail will be sent unencrypted.</label>` : ""}
  </fieldset>`;

function readSetupForm() {
  const v = (n) => $(`#modal [name="${n}"]`);
  if (!v("name")) return;
  setup.name = v("name").value;
  setup.username = v("username").value;
  setup.password = v("password").value;
  for (const [kind, ep] of [["in", setup.incoming], ["smtp", setup.smtp]]) {
    ep.host = v(`${kind}-host`).value.trim();
    ep.port = +v(`${kind}-port`).value;
    ep.allow_insecure = !!v(`${kind}-insecure`)?.checked;
  }
}

function renderSetup() {
  const s = setup;
  if (s.step === "email") {
    put("#modal", modal("Add an account", html`
      <div class="oauth-btns">
        <button class="btn" data-click="oauth" data-provider="google" data-what="account">Sign in with Google</button>
        <button class="btn" data-click="oauth" data-provider="microsoft" data-what="account">Sign in with Microsoft (Outlook.com, Microsoft 365)</button>
      </div>
      <p class="divider">or use any other email with a password</p>
      <form class="form" data-submit="setup-next">
        <label class="stack"><span>Email address</span><input name="email" type="email" value="${s.email}" autocomplete="email" required /></label>
        ${s.error ? html`<div class="error">${s.error}</div>` : ""}
        <button class="btn primary" type="submit">Next</button>
      </form>
      <p class="muted small center">Moving from another device or reinstalling? <button class="linkish" data-click="restore-open">Restore from a backup</button></p>`, "", s.canCancel));
    $("#modal [name=email]").focus();
    return;
  }
  put("#modal", modal("Add an account", html`
    <div class="form">
      ${s.note ? html`<div class="note">${s.note}</div>` : ""}
      <label class="stack"><span>Your name</span><input name="name" value="${s.name}" /></label>
      <label class="stack"><span>User name</span><input name="username" value="${s.username}" autocapitalize="off" /></label>
      <label class="stack"><span>Password</span><input name="password" type="password" value="${s.password ?? ""}" autocomplete="current-password" /></label>
      <label class="stack"><span>Incoming mail</span>
        <select data-change="setup-protocol">
          ${options([["imap", "IMAP (recommended: syncs folders across devices)"], ["pop3", "POP3 (downloads to this device only)"]], s.protocol)}
        </select></label>
      ${serverBlock(s.protocol === "imap" ? "IMAP server" : "POP3 server", s.incoming, "in")}
      ${serverBlock("SMTP server (outgoing)", s.smtp, "smtp")}
      ${s.error ? html`<div class="error">${s.error}</div>` : ""}
    </div>`,
    html`<button class="btn" data-click="setup-back">Back</button><button class="btn primary" data-click="setup-save">Sign in</button>`, s.canCancel));
}

on("submit", "setup-next", async (el, e) => {
  e.preventDefault();
  const email = $("#modal [name=email]").value.trim();
  setup.email = email;
  if (!email.includes("@")) { setup.error = "Enter a valid email address"; return renderSetup(); }
  const p = await api.guessSettings(email);
  Object.assign(setup, {
    step: "details", error: null, protocol: p.incoming_protocol, note: p.note,
    incoming: { ...p.incoming, allow_insecure: false }, smtp: { ...p.smtp, allow_insecure: false },
    username: setup.username || email, name: setup.name || email.split("@")[0],
  });
  renderSetup();
});
on("change", "setup-protocol", (el) => {
  readSetupForm();
  setup.protocol = el.value;
  setup.incoming.port = PORTS[el.value][setup.incoming.security];
  renderSetup();
});
on("change", "setup-security", (el) => {
  readSetupForm();
  const kind = el.dataset.kind;
  const ep = kind === "in" ? setup.incoming : setup.smtp;
  ep.security = el.value;
  ep.port = PORTS[kind === "in" ? setup.protocol : "smtp"][el.value];
  renderSetup();
});
on("click", "setup-back", () => { readSetupForm(); setup.step = "email"; renderSetup(); });
on("click", "setup-save", async (el) => {
  readSetupForm();
  el.disabled = true;
  el.textContent = "Checking connection…";
  const s = setup;
  try {
    const account = await api.addAccount({
      id: 0, name: s.name, email: s.email, username: s.username, incoming_protocol: s.protocol,
      incoming: s.incoming, smtp: s.smtp, smtp_username: null, signature: "",
    }, s.password ?? "", true);
    closeModal();
    S.accounts = [...S.accounts, account];
    await selectAccount(account);
  } catch (e) {
    s.error = String(e);
    renderSetup();
  }
});

// ---- settings ----------------------------------------------------------------------

// Settings: four sections side by side that you swipe (or tap a title) between.
const PIVOTS = [["general", "general"], ["security", "security & privacy"], ["data", "your data"], ["about", "about"]];
const hhmm = (m) => `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
const DAYS = [[0, "Mon"], [1, "Tue"], [2, "Wed"], [3, "Thu"], [4, "Fri"], [5, "Sat"], [6, "Sun"]];

export async function openSettings(start = "general") {
  const [scanner, usage, app, engine, upd, trk] = await Promise.all([api.getScanner(), api.storageUsage(), api.getSettings(), api.scannerStatus(), api.updateStatus(), api.trackerStatus()]);
  put("#modal", html`
  <div class="modal-backdrop" data-click="modal-backdrop">
    <div class="modal settings" role="dialog" aria-label="Settings">
      <header><span>settings</span><button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button></header>
      <nav class="pivot-titles" role="tablist">
        ${PIVOTS.map(([k, l]) => html`<button role="tab" data-click="pivot" data-k="${k}">${l}</button>`)}
      </nav>
      <div class="pivots">
        <section class="pivot form" data-k="general">
          <h3>Accounts</h3>
          <div><button class="btn" data-click="accounts">Manage accounts…</button></div>
          <h3>Checking for new mail</h3>
          <label class="check">During working hours, every
            <select name="sync-minutes" data-change="app-autosave">${[5, 10, 15, 30, 60].map((m) => html`<option value="${m}" ${app.sync_minutes === m ? "selected" : ""}>${m} minutes</option>`)}</select></label>
          <div class="workdays">${DAYS.map(([d, l]) => html`<label class="day"><input type="checkbox" name="work-day" value="${d}" ${app.work_days.includes(d) ? "checked" : ""} data-change="app-autosave" /><span>${l}</span></label>`)}</div>
          <div class="row2"><label class="stack"><span>From</span><input type="time" name="work-start" value="${hhmm(app.work_start)}" data-change="app-autosave" /></label>
            <label class="stack"><span>Until</span><input type="time" name="work-end" value="${hhmm(app.work_end)}" data-change="app-autosave" /></label></div>
          <label class="check">Outside working hours
            <select name="off-hours" data-change="app-autosave">${[[60, "every hour"], [180, "every 3 hours"], [0, "only when I ask"]].map(([m, l]) => html`<option value="${m}" ${app.off_hours_minutes === m ? "selected" : ""}>${l}</option>`)}</select></label>
          <p class="muted small">The sync button checks right away at any time.</p>
          <h3>Swiping a conversation</h3>
          <div class="row2 swipe-prefs">
            ${["left", "right"].map((side) => html`<label class="stack"><span>Swipe ${side === "left" ? "left ←" : "right →"}</span>
              <select data-change="swipe-pref" data-side="${side}">${SWIPE_ACTIONS.map(([k, l]) => html`<option value="${k}" ${swipeAction(side) === k ? "selected" : ""}>${l}</option>`)}</select></label>`)}
          </div>
          <p class="muted small">On touch screens. Press and hold a conversation to pick several, then tap more.</p>
          <h3>Notifications</h3>
          <label class="check"><input type="checkbox" name="notify" ${app.notify ? "checked" : ""} data-change="app-autosave" /> Notify me when new mail arrives</label>
          <label class="check hide-narrow"><input type="checkbox" name="close-to-tray" ${app.close_to_tray ? "checked" : ""} data-change="app-autosave" /> Closing the window keeps Carteiro running in the system tray</label>
          <h3>Calendar</h3>
          <label class="stack narrow-field"><span>Week starts on</span>
            <select data-change="week-start">
              ${[[0, "Sunday"], [1, "Monday"], [6, "Saturday"]].map(([v, l]) => html`<option value="${v}" ${weekStart() === v ? "selected" : ""}>${l}</option>`)}
            </select></label>
        </section>

        <section class="pivot form" data-k="security">
          <h3>${icon("shield", 16)} Malware scanning (ClamAV)</h3>
          <p class="muted">Every message and calendar is scanned before its formatted content or attachments are shown. Dangerous attachment types and deceptive links are always flagged.</p>
          <label class="check"><input type="checkbox" name="scan-enabled" ${scanner.enabled ? "checked" : ""} /> Scan messages and calendars</label>
          <label class="check"><input type="radio" name="scan-mode" value="builtin" ${scanner.builtin ? "checked" : ""} data-change="scan-mode" /> Built-in ClamAV engine</label>
          <div class="engine-status small ${engine.state}" data-show="builtin" ${scanner.builtin ? "" : "hidden"}>${engineStatus(engine)}
            <button class="btn" data-click="scanner-update">Update signatures now</button></div>
          <label class="check"><input type="radio" name="scan-mode" value="external" ${scanner.builtin ? "" : "checked"} data-change="scan-mode" /> A clamd server I run myself</label>
          <div data-show="external" ${scanner.builtin ? "hidden" : ""} class="stack">
            <label class="stack narrow-field"><span>clamd address</span><input name="scan-addr" value="${scanner.builtin ? "" : scanner.clamd_addr}" placeholder="127.0.0.1:3310" /></label>
            <div><button class="btn" data-click="scanner-test">Test connection</button></div>
          </div>
          <label class="check"><input type="checkbox" name="scan-block" ${scanner.block_when_unavailable ? "checked" : ""} /> Until a message has been scanned, hide its formatted content and attachments (recommended)</label>
          <div class="btn-row"><button class="btn primary" data-click="scanner-save">Save</button></div>
          <div class="scan-result small" hidden></div>
          <h3>Privacy</h3>
          <p class="muted small">Images from the web load only when you ask, never from trackers, and links lose their tracking parameters. Scripts in mail never run.</p>
          <div class="tracker-status small">${trackerLine(trk)}</div>
          <div><button class="btn" data-click="trackers-update">Update tracker list now</button></div>
        </section>

        <section class="pivot form" data-k="data">
          <h3>Offline mail</h3>
          <label class="stack narrow-field"><span>Keep mail available offline for</span>
            <select name="offline-days" data-change="app-autosave">
              ${[[1, "The last day"], [7, "The last week"], [30, "The last month"], [61, "The last 2 months"], [183, "The last 6 months"], [365, "The last year"], [0, "All mail"]].map(([d, l]) => html`<option value="${d}" ${app.offline_days === d ? "selected" : ""}>${l}</option>`)}
            </select></label>
          <p class="muted small">Messages in this period are downloaded in full so they open without a connection. Older ones stay listed and download when opened.</p>
          <div class="btn-row wrap"><button class="btn" data-click="offline-now">Download now</button></div>
          <h3>Backup</h3>
          <p class="muted small">One file with all your mail, contacts, calendars, settings and passwords, encrypted with a passphrase you choose. Restore it on any device or a new install to carry on where you left off.</p>
          <div class="btn-row wrap">
            <button class="btn" data-click="backup-open">Back up everything…</button>
            <button class="btn" data-click="restore-open">Restore from a backup…</button>
          </div>
          <h3>Export</h3>
          <div class="btn-row wrap">
            <button class="btn" data-click="export-mail">Save all mail to disk (.mbox)</button>
            <button class="btn" data-click="export-settings">Export settings…</button>
            <button class="btn" data-click="import-settings">Import settings…</button>
            <input type="file" name="import-file" accept=".json,application/json" hidden data-change="import-file" />
          </div>
          <p class="muted small">Settings exports contain accounts, rules, calendars and preferences, but never passwords or sign-in tokens.</p>
          <h3>Storage</h3>
          <p class="muted small">Identical attachments are stored once.
            ${usage[0] ? `Attachments: ${bytes(usage[1])} stored for ${bytes(usage[0])} of content (${Math.round((1 - usage[1] / usage[0]) * 100)}% saved).` : ""}</p>
        </section>

        <section class="pivot form" data-k="about">
          <h3>Updates</h3>
          <p class="muted small">Carteiro ${upd.current}. Updates come from Carteiro's GitHub releases, must carry the release signature and pass the virus scan before they're installed.</p>
          <label class="check"><input type="checkbox" name="auto-update" ${app.auto_update ? "checked" : ""} ${upd.enabled ? "" : "disabled"} data-change="app-autosave" /> Download and install updates automatically</label>
          <div class="update-status small">${updateStatus(upd)}</div>
          <div class="btn-row wrap">
            <button class="btn" data-click="update-check" ${upd.enabled ? "" : "disabled"}>Check now</button>
            <button class="btn primary" data-click="update-install" ${upd.available ? "" : "hidden"}>${upd.ready ? "Restart to update" : "Get update"}</button>
          </div>
          <h3>About</h3>
          <div class="about"></div>
        </section>
      </div>
    </div>
  </div>`);
  showPivot(start, false);
  renderAbout(upd.current);
}

function showPivot(k, smooth = true) {
  const pane = $(`#modal .pivot[data-k="${k}"]`);
  pane?.parentElement.scrollTo({ left: pane.offsetLeft - pane.parentElement.offsetLeft, behavior: smooth ? "smooth" : "instant" });
  markPivot(k);
}
// The current title leads the strip, the others follow dimmed (and wrap around).
function markPivot(k) {
  const nav = $("#modal .pivot-titles");
  if (!nav) return;
  const i = PIVOTS.findIndex(([x]) => x === k);
  for (const b of nav.children) {
    const j = PIVOTS.findIndex(([x]) => x === b.dataset.k);
    b.classList.toggle("on", j === i);
    b.setAttribute("aria-selected", j === i);
    b.style.order = (j - i + PIVOTS.length) % PIVOTS.length;
  }
}
on("click", "pivot", (el) => showPivot(el.dataset.k));
document.addEventListener("scroll", (e) => {
  const box = e.target;
  if (!(box instanceof Element) || !box.matches?.("#modal .pivots")) return;
  const i = Math.round(box.scrollLeft / box.clientWidth);
  if (PIVOTS[i]) markPivot(PIVOTS[i][0]);
}, true);
document.addEventListener("keydown", (e) => {
  if (!$("#modal .settings") || !["ArrowLeft", "ArrowRight"].includes(e.key) || e.target.closest("input, select, textarea")) return;
  const cur = PIVOTS.findIndex(([k]) => k === $("#modal .pivot-titles .on")?.dataset.k);
  const next = PIVOTS[(cur + (e.key === "ArrowRight" ? 1 : PIVOTS.length - 1)) % PIVOTS.length][0];
  showPivot(next);
});

// Settings > About: version, licenses of every component (ui/licenses.json is
// generated from the build by scripts/gen-licenses.py) and the source code.
async function renderAbout(version) {
  let lic;
  try { lic = await (await fetch("licenses.json")).json(); } catch { return; }
  const row = (c) => html`<li><button class="linkish" data-click="open-url" data-url="${c.url}">${c.name}</button> ${c.version} <span class="muted">· ${c.license}${c.note ? ` · ${c.note}` : ""}</span></li>`;
  put("#modal .about", html`
    <p><strong>${lic.app.name} ${version}</strong>, free software under the ${lic.app.license} license.
      <button class="linkish" data-click="open-url" data-url="${lic.app.url}">Source code on GitHub</button></p>
    <p class="muted small">It's built with these components, each under its own license:</p>
    <ul class="licenses">${lic.bundled.map(row)}</ul>
    <details><summary>${lic.crates.length} Rust libraries (Windows, Linux and Android builds together, including build tools)</summary>
      <ul class="licenses">${lic.crates.map(row)}</ul></details>
    <p class="muted small">The interface uses the system web view (WebView2, WebKitGTK or Android System WebView) and no third-party web code.</p>`);
}
on("click", "open-url", (el) => api.openLink(el.dataset.url).catch((e) => toast(String(e))));

function trackerLine(t) {
  const when = t.updated ? new Date(t.updated * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" }) : null;
  return html`<span>${(t.builtin + t.downloaded).toLocaleString()} tracker hosts blocked: ${t.builtin.toLocaleString()} built in,
    ${t.downloaded ? html`${t.downloaded.toLocaleString()} mail trackers from AdGuard (updated ${when})` : "AdGuard's mail tracker list not downloaded yet"}.</span>`;
}
on("click", "trackers-update", async (el) => {
  el.disabled = true;
  el.textContent = "Updating…";
  try { $("#modal .tracker-status").innerHTML = String(trackerLine(await api.updateTrackers())); }
  catch (e) { toast(String(e)); }
  el.disabled = false;
  el.textContent = "Update tracker list now";
});

function updateStatus(u) {
  return html`<span class="muted">${u.message}</span>`;
}

on("click", "update-check", async (el) => {
  el.disabled = true;
  el.textContent = "Checking…";
  let u;
  try { u = await api.checkUpdate(); } catch (e) { u = await api.updateStatus(); }
  $("#modal .update-status").innerHTML = String(updateStatus(u));
  const install = $("#modal [data-click=update-install]");
  install.hidden = !u.available;
  install.textContent = u.ready ? "Restart to update" : "Get update";
  el.disabled = false;
  el.textContent = "Check now";
});
on("click", "update-install", async () => {
  try { await api.installUpdate(); } catch (e) { toast(String(e)); }
});

function engineStatus(e) {
  const label = { ready: "Running", updating: "Updating", starting: "Starting", missing: "Not included", error: "Problem" }[e.state] ?? e.state;
  return html`<span><strong>${label}</strong>${e.version ? ` · ${e.version}` : ""}${e.detail ? html`<br /><span class="muted">${e.detail}</span>` : ""}</span>`;
}

on("change", "scan-mode", (el) => {
  for (const d of document.querySelectorAll("#modal [data-show]")) d.hidden = d.dataset.show !== el.value;
});
on("click", "scanner-update", async (el) => {
  el.disabled = true;
  el.textContent = "Updating…";
  try {
    const st = await api.scannerUpdateNow();
    const box = $("#modal .engine-status");
    box.className = `engine-status small ${st.state}`;
    box.firstChild.replaceWith(document.createRange().createContextualFragment(String(engineStatus(st))));
  } catch (e) { toast(String(e)); }
  el.disabled = false;
  el.textContent = "Update signatures now";
});

on("click", "scanner-save", async (el) => {
  const v = (n) => $(`#modal [name=${n}]`);
  try {
    const builtin = $("#modal [name=scan-mode]:checked").value === "builtin";
    await api.setScanner({ enabled: v("scan-enabled").checked, builtin, clamd_addr: builtin ? "" : v("scan-addr").value.trim(), block_when_unavailable: v("scan-block").checked });
    el.textContent = "Saved";
    setTimeout(() => (el.textContent = "Save"), 2000);
  } catch (e) { toast(String(e)); }
});

on("change", "swipe-pref", (el) => { try { localStorage.setItem(`carteiro.swipe.${el.dataset.side}`, el.value); } catch {} });

// Settings save as soon as they change.
const minutesOf = (t) => { const [h, m] = (t || "0:0").split(":").map(Number); return h * 60 + m; };
on("change", "app-autosave", async () => {
  const v = (n) => $(`#modal [name=${n}]`);
  try {
    await api.setSettings({
      close_to_tray: v("close-to-tray").checked,
      notify: v("notify").checked,
      sync_minutes: +v("sync-minutes").value || 15,
      work_days: [...document.querySelectorAll("#modal [name=work-day]:checked")].map((x) => +x.value),
      work_start: minutesOf(v("work-start").value),
      work_end: minutesOf(v("work-end").value),
      off_hours_minutes: +v("off-hours").value,
      schedule_version: 1,
      offline_days: +v("offline-days").value,
      auto_update: v("auto-update").checked,
    });
  } catch (e) { toast(String(e)); }
});
on("click", "scanner-test", async (el) => {
  const out = $("#modal .scan-result");
  el.disabled = true;
  out.hidden = false;
  out.className = "scan-result small muted";
  out.textContent = "Contacting clamd…";
  try {
    const version = await api.testScanner($("#modal [name=scan-addr]").value.trim());
    out.className = "scan-result small ok";
    out.textContent = `Connected: ${version}`;
  } catch (e) {
    out.className = "scan-result small error";
    out.textContent = String(e);
  }
  el.disabled = false;
});

// ---- accounts ------------------------------------------------------------------------

let editingAccount = null;

let calSources = [];

export async function openAccounts() {
  editingAccount = null;
  calSources = (await api.calSources()) ?? [];
  renderAccounts();
}

function renderAccounts(error = null) {
  const a = editingAccount;
  if (!a) {
    put("#modal", modal("Accounts", html`
      ${S.accounts.map((x) => html`
        <div class="rule">
          <div class="rule-main"><div class="rule-name">${x.name} &lt;${x.email}&gt;</div>
            <div class="muted small">${x.incoming_protocol.toUpperCase()} ${x.incoming.host}:${x.incoming.port} (${x.incoming.security}) · SMTP ${x.smtp.host}:${x.smtp.port} (${x.smtp.security}) · user ${x.username}</div></div>
          <button class="btn" data-click="account-edit" data-id="${x.id}">Edit</button>
          <button class="btn danger" data-click="account-remove" data-id="${x.id}">Remove</button>
        </div>`)}
      <h3 class="sect">Calendars</h3>
      ${calSources.filter((c) => c.kind !== "local" && c.account_id == null).map((c) => html`
        <div class="rule"><div class="rule-main"><div class="rule-name">${c.name}</div>
          <div class="muted small">${{ caldav: "CalDAV", google: "Google", microsoft: "Microsoft 365 / Outlook" }[c.kind] ?? c.kind}${c.url ? ` · ${c.url}` : ""}</div></div>
          <button class="btn danger" data-click="cal-source-remove" data-id="${c.id}">Remove</button></div>`)}
      <p class="muted small">Mail accounts that signed in with Google or Microsoft include their calendar automatically. Add a calendar on its own, e.g. a Microsoft 365 work calendar next to an IMAP mailbox:</p>
      <div class="btn-row wrap">
        <button class="btn" data-click="oauth" data-provider="microsoft" data-what="calendar">+ Microsoft 365 / Outlook calendar</button>
        <button class="btn" data-click="oauth" data-provider="google" data-what="calendar">+ Google calendar</button>
        <button class="btn" data-click="caldav-add">+ CalDAV calendar</button>
      </div>`,
      html`<button class="btn primary" data-click="add-account">+ Add account</button>`));
    return;
  }
  put("#modal", modal(`Edit ${a.email}`, html`
    <div class="form">
      <label class="stack"><span>Display name</span><input name="acct-name" value="${a.name}" /></label>
      <label class="stack"><span>Signature</span><textarea name="acct-sig" rows="4">${a.signature}</textarea></label>
      <label class="stack"><span>SMTP user name (leave empty to use "${a.username}")</span><input name="acct-smtp-user" value="${a.smtp_username ?? ""}" /></label>
      <label class="stack"><span>New password (leave empty to keep the current one)</span><input name="acct-pw" type="password" autocomplete="new-password" /></label>
      ${error ? html`<div class="error">${error}</div>` : ""}
    </div>`,
    html`<button class="btn" data-click="account-back">Cancel</button><button class="btn primary" data-click="account-save">Save</button>`));
}

on("click", "accounts", () => {
  const m = $("#acct-menu");
  if (m) m.hidden = true;
  openAccounts();
});
on("click", "cal-source-remove", async (el) => {
  if (!(await ask("Remove this calendar from Carteiro? Events on the server are not affected.", "Remove"))) return;
  await api.removeCalSource(+el.dataset.id);
  openAccounts();
});
on("click", "caldav-add", () => {
  put("#modal", modal("Add CalDAV calendar", html`
    <div class="form">
      <label class="stack"><span>Server URL</span><input name="dav-url" placeholder="https://cloud.example.com/remote.php/dav" autocapitalize="off" spellcheck="false" /></label>
      <label class="stack"><span>User name</span><input name="dav-user" autocapitalize="off" /></label>
      <label class="stack"><span>Password (or app password)</span><input name="dav-pw" type="password" /></label>
      <label class="stack"><span>Name (optional)</span><input name="dav-name" /></label>
      <div class="error" hidden></div>
    </div>`,
    html`<button class="btn" data-click="accounts">Cancel</button><button class="btn primary" data-click="caldav-save">Add</button>`));
});
on("click", "caldav-save", async (el) => {
  const v = (n) => $(`#modal [name=${n}]`).value.trim();
  el.disabled = true;
  el.textContent = "Connecting…";
  try {
    await api.addCaldav(v("dav-url"), v("dav-user"), $("#modal [name=dav-pw]").value, v("dav-name"));
    toast("Calendar added");
    window.dispatchEvent(new Event("calendars-changed"));
    if ($("#calendar").hidden) openAccounts(); else closeModal();
  } catch (e) {
    const err = $("#modal .error");
    err.textContent = String(e);
    err.hidden = false;
    el.disabled = false;
    el.textContent = "Add";
  }
});

// Google / Microsoft sign-in: the system browser opens; we wait for it.
on("click", "oauth", async (el) => {
  const { provider, what } = el.dataset;
  const label = el.textContent;
  for (const b of document.querySelectorAll("#modal button")) b.disabled = true;
  el.textContent = "Waiting for the browser…";
  try {
    const r = await api.oauthSignIn(provider, what);
    if (r.kind === "account") {
      closeModal();
      S.accounts = [...S.accounts, r.account];
      await selectAccount(r.account);
      toast(`Signed in as ${r.account.email}`);
    } else {
      toast("Calendar added; syncing…");
      window.dispatchEvent(new Event("calendars-changed"));
      if ($("#calendar").hidden) openAccounts(); else closeModal();
    }
  } catch (e) {
    toast(String(e));
    for (const b of document.querySelectorAll("#modal button")) b.disabled = false;
    el.textContent = label;
  }
});
on("click", "account-edit", (el) => { editingAccount = structuredClone(accountById(+el.dataset.id)); renderAccounts(); });
on("click", "account-back", () => { editingAccount = null; renderAccounts(); });
on("click", "account-save", async (el) => {
  const v = (n) => $(`#modal [name=${n}]`).value;
  const a = { ...editingAccount, name: v("acct-name"), signature: v("acct-sig"), smtp_username: v("acct-smtp-user").trim() || null };
  el.disabled = true;
  el.textContent = v("acct-pw") ? "Checking password…" : "Saving…";
  try {
    const saved = await api.updateAccount(a, v("acct-pw") || null);
    S.accounts = S.accounts.map((x) => (x.id === saved.id ? saved : x));
    if (S.account?.id === saved.id) S.account = saved;
    editingAccount = null;
    renderAccounts();
    toast("Account saved");
  } catch (e) {
    renderAccounts(String(e));
  }
});
on("click", "account-remove", async (el) => {
  const a = accountById(+el.dataset.id);
  if (!a || !(await ask(`Remove ${a.email} from this device? Mail on the server is not affected.`, "Remove"))) return;
  await api.removeAccount(a.id);
  if (S.account?.id === a.id) S.account = null;
  S.accounts = S.accounts.filter((x) => x.id !== a.id);
  if (S.accounts.length) renderAccounts();
  else closeModal(); // loadAccounts clears the screen and opens setup
  loadAccounts();
});

on("change", "week-start", (el) => {
  try { localStorage.setItem(WEEK_START_KEY, el.value); } catch {}
  refreshCalendar();
});

// ---- your data ---------------------------------------------------------------------

async function busy(el, label, work) {
  const old = el.textContent;
  el.disabled = true;
  el.textContent = label;
  try {
    return await work();
  } catch (e) {
    toast(String(e));
  } finally {
    el.disabled = false;
    el.textContent = old;
  }
}

on("click", "offline-now", (el) => busy(el, "Downloading…", async () => {
  // Save the chosen period first so "Download now" uses it.
  // (the chosen period was already saved when it changed)
  const r = await api.downloadOfflineNow();
  toast(`Offline copy updated: ${r.downloaded} downloaded, ${r.freed} older ones freed`);
}));

on("click", "export-mail", (el) => busy(el, "Saving…", async () => {
  const [dir, reports] = await api.exportMail();
  const n = reports.reduce((a, r) => a + r.messages, 0);
  const skipped = reports.reduce((a, r) => a + r.skipped, 0);
  toast(`Saved ${n} messages to ${dir}${skipped ? ` (${skipped} couldn't be downloaded)` : ""}`);
}));

const uiPrefs = () => {
  const get = (k) => { try { return localStorage.getItem(k); } catch { return null; } };
  return { theme: get("carteiro.theme"), week_start: get(WEEK_START_KEY), swipe_left: get("carteiro.swipe.left"), swipe_right: get("carteiro.swipe.right") };
};

const IS_ANDROID = !!window.CarteiroAndroid;

// A full backup is encrypted with a passphrase; nobody can open it without it,
// including us, so there is no way to recover a forgotten one.
on("click", "backup-open", () => put("#modal", html`
  <div class="modal-backdrop" data-click="modal-backdrop">
    <div class="modal" role="dialog" aria-label="Back up everything">
      <header>Back up everything<button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button></header>
      <div class="content form">
        <p class="muted small">Saves your mail, contacts, calendars, settings and passwords in one encrypted file. Keep it somewhere other than this device: a USB drive, another computer, or cloud storage.</p>
        <label class="stack"><span>Passphrase (at least 8 characters)</span><input name="pass" type="password" autocomplete="new-password" /></label>
        <label class="stack"><span>Repeat the passphrase</span><input name="pass2" type="password" autocomplete="new-password" /></label>
        <p class="muted small">Without the passphrase the backup can't be opened, by anyone. Write it down.</p>
        ${IS_ANDROID ? "" : html`<label class="stack"><span>Folder (empty: Downloads/Carteiro)</span><input name="folder" placeholder="for example /media/usb or D:\\Backups" autocapitalize="off" /></label>`}
        ${IS_ANDROID ? html`<p class="muted small">It goes to Download/Carteiro, which stays even if the app is uninstalled. Copy it off the phone too.</p>` : ""}
        <div class="error" hidden></div>
      </div>
      <footer><span class="spacer"></span><button class="btn" data-click="modal-close">Cancel</button><button class="btn primary" data-click="backup-go">Back up</button></footer>
    </div>
  </div>`));

const modalError = (msg) => { const e = $("#modal .error"); e.textContent = String(msg); e.hidden = !msg; };

on("click", "backup-go", (el) => {
  const v = (n) => $(`#modal [name=${n}]`)?.value ?? "";
  if (v("pass").length < 8) return modalError("Use a passphrase of at least 8 characters.");
  if (v("pass") !== v("pass2")) return modalError("The two passphrases are different.");
  modalError("");
  return busy(el, "Backing up…", async () => {
    try {
      const path = await api.createBackup(v("folder").trim() || null, v("pass"), uiPrefs());
      put("#modal", "");
      toast(`Backup saved to ${path}`);
    } catch (e) { modalError(e); }
  });
});

on("click", "restore-open", () => put("#modal", html`
  <div class="modal-backdrop" data-click="modal-backdrop">
    <div class="modal" role="dialog" aria-label="Restore from a backup">
      <header>Restore from a backup<button class="icon-btn" aria-label="Close" data-click="modal-close">${icon("close")}</button></header>
      <div class="content form">
        <p class="error-text">Everything currently in Carteiro on this device is replaced by the backup.</p>
        <label class="stack"><span>Backup file (.carteiro)</span><input name="file" type="file" accept=".carteiro,application/octet-stream" /></label>
        <label class="stack"><span>Passphrase</span><input name="pass" type="password" autocomplete="current-password" /></label>
        <div class="error" hidden></div>
      </div>
      <footer><span class="spacer"></span><button class="btn" data-click="modal-close">Cancel</button><button class="btn danger" data-click="restore-go">Replace and restore</button></footer>
    </div>
  </div>`));

on("click", "restore-go", (el) => {
  const file = $("#modal [name=file]").files?.[0];
  const pass = $("#modal [name=pass]").value;
  if (!file) return modalError("Choose the backup file.");
  if (!pass) return modalError("Enter the backup's passphrase.");
  modalError("");
  return busy(el, "Restoring…", async () => {
    try {
      const r = await api.restoreBackup(file, pass, (f) => { el.textContent = `Reading ${Math.round(f * 100)}%…`; });
      try {
        if (r.ui?.theme) localStorage.setItem("carteiro.theme", r.ui.theme);
        if (r.ui?.week_start != null) localStorage.setItem(WEEK_START_KEY, r.ui.week_start);
        for (const side of ["left", "right"]) if (r.ui?.[`swipe_${side}`]) localStorage.setItem(`carteiro.swipe.${side}`, r.ui[`swipe_${side}`]);
      } catch {}
      toast(`Restored ${r.accounts} account${r.accounts === 1 ? "" : "s"}`);
      setTimeout(() => location.reload(), 1200);
    } catch (e) { modalError(e); }
  });
});

on("click", "export-settings", (el) => busy(el, "Exporting…", async () => {
  toast(`Settings saved to ${await api.exportSettings(uiPrefs())}`);
}));

on("click", "import-settings", () => $("#modal [name=import-file]").click());
on("change", "import-file", async (el) => {
  const file = el.files?.[0];
  el.value = "";
  if (!file) return;
  try {
    const { report: r, ui } = await api.importSettings(await file.text());
    try {
      if (ui?.theme) localStorage.setItem("carteiro.theme", ui.theme);
      if (ui?.week_start != null) localStorage.setItem(WEEK_START_KEY, ui.week_start);
      for (const side of ["left", "right"]) if (ui?.[`swipe_${side}`]) localStorage.setItem(`carteiro.swipe.${side}`, ui[`swipe_${side}`]);
    } catch {}
    const lines = [
      r.accounts_added.length && `Added accounts: ${r.accounts_added.join(", ")}`,
      r.accounts_existing.length && `Already here (unchanged): ${r.accounts_existing.join(", ")}`,
      r.needs_password.length && `Enter the password for: ${r.needs_password.join(", ")} (Accounts → Edit)`,
      r.needs_sign_in.length && `Sign in again for: ${r.needs_sign_in.join(", ")}`,
      `Rules added: ${r.rules_added}. Calendars added: ${r.calendars_added}.`,
      r.calendars_to_readd.length && `Add again by hand (they need credentials): ${r.calendars_to_readd.join(", ")}`,
    ].filter(Boolean);
    put("#modal", modal("Settings imported", html`<ul class="import-report">${lines.map((l) => html`<li>${l}</li>`)}</ul>`,
      html`<button class="btn primary" data-click="accounts">Open accounts</button>`));
    loadAccounts();
  } catch (e) {
    toast(String(e));
  }
});
