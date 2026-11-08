// Tiny DOM toolkit: an auto-escaping `html` template tag and delegated events.

class Raw {
  constructor(s) { this.s = s; }
  toString() { return this.s; }
}

export const raw = (s) => new Raw(s);

const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
export const esc = (v) => String(v).replace(/[&<>"']/g, (c) => ESC[c]);

function fmt(v) {
  if (v == null || v === false) return "";
  if (Array.isArray(v)) return v.map(fmt).join("");
  if (v instanceof Raw) return v.s;
  return esc(v);
}

/** Interpolations are escaped unless wrapped in raw() or produced by html``. */
export function html(strings, ...vals) {
  let out = strings[0];
  vals.forEach((v, i) => (out += fmt(v) + strings[i + 1]));
  return new Raw(out);
}

export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

export function put(el, content) {
  if (typeof el === "string") el = $(el);
  if (el) el.innerHTML = fmt(content);
  return el;
}

// Delegated handlers: elements declare data-<event>="name" and we dispatch to
// handlers[event][name](element, event).
const handlers = {};
export function on(event, name, fn) {
  (handlers[event] ??= {})[name] = fn;
}
for (const ev of ["click", "change", "input", "keydown", "submit", "dragstart", "dragover", "dragleave", "drop"]) {
  document.addEventListener(ev, (e) => {
    const el = e.target.closest?.(`[data-${ev}]`);
    if (!el) return;
    const fn = handlers[ev]?.[el.dataset[ev]];
    if (fn) fn(el, e);
  });
}

/// An in-app yes/no question. Web views can't be relied on to show the
/// browser's own `confirm()` (Android's may not show it at all), so every
/// confirmation goes through this. Resolves true for the confirming button.
export function ask(question, yes = "Delete", danger = true) {
  return new Promise((resolve) => {
    const box = document.createElement("div");
    box.className = "ask-backdrop";
    box.innerHTML = fmt(html`<div class="ask" role="alertdialog" aria-label="${question}">
      <p>${question}</p>
      <div class="btn-row"><span class="spacer"></span><button class="btn" data-ask="no">Cancel</button><button class="btn ${danger ? "danger" : "primary"}" data-ask="yes">${yes}</button></div>
    </div>`);
    const done = (v) => { box.remove(); document.removeEventListener("keydown", key, true); resolve(v); };
    const key = (e) => { if (e.key === "Escape") { e.stopPropagation(); done(false); } };
    box.addEventListener("click", (e) => {
      const b = e.target.closest("[data-ask]");
      if (b) done(b.dataset.ask === "yes");
      else if (e.target === box) done(false);
    });
    document.addEventListener("keydown", key, true);
    document.body.append(box);
    box.querySelector("[data-ask=yes]").focus();
  });
}

/// Like `ask`, with several answers: resolves the chosen key, or null.
/// `extra` is shown under the answers (it may hold more of them, with inputs);
/// `read(box)` collects them when an answer is picked; `intro` goes right
/// under the question (what the answers are about).
export function choose(question, options, extra = "", read = () => null, intro = "") {
  return new Promise((resolve) => {
    const box = document.createElement("div");
    box.className = "ask-backdrop";
    box.innerHTML = fmt(html`<div class="ask" role="alertdialog" aria-label="${question}">
      <p>${question}</p>${intro}
      <div class="choices">${options.map(([k, l, danger]) => html`<button class="btn ${danger ? "danger" : ""}" data-ask="${k}">${l}</button>`)}</div>
      ${extra}
      <div class="choices"><button class="btn" data-ask="">Cancel</button></div>
    </div>`);
    const done = (v) => { box.remove(); document.removeEventListener("keydown", key, true); resolve(v); };
    const key = (e) => { if (e.key === "Escape") { e.stopPropagation(); done(null); } };
    box.addEventListener("click", (e) => {
      const b = e.target.closest("[data-ask]");
      if (b) done(b.dataset.ask ? { choice: b.dataset.ask, value: read(box) } : null);
      else if (e.target === box) done(null);
    });
    document.addEventListener("keydown", key, true);
    document.body.append(box);
  });
}
