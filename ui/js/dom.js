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
