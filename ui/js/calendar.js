// Calendar: day / week / month views on desktop, an agenda list on phones,
// and the event editor.

import { api } from "./api.js";
import { $, ask, choose, html, on, put, raw } from "./dom.js";
import { icon } from "./format.js";
import { closeDrawer, showMessage, toast } from "./app.js";

const C = {
  view: null, // "day" | "week" | "month" | "agenda"
  anchor: startOfDay(new Date()),
  calendars: [],
  events: [],
  editing: null, // { id, calendar_id, title, ... } while the dialog is open
  syncing: false,
  slide: null, // "next" | "prev" after a swipe, for the slide-in animation
  keepWeek: null, // phone month view: week (timestamp) to keep at the top after reloading
  query: "", // event search; non-empty shows results instead of the view
  results: [],
  loads: 0, // only the latest load may draw (navigation can overtake a slower one)
};

const HOUR_PX = 48;
const narrow = () => window.matchMedia("(max-width: 720px), (max-height: 520px) and (pointer: coarse)").matches;

// ---- dates -----------------------------------------------------------------------

function startOfDay(d) {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}
const addDays = (d, n) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
const sameDay = (a, b) => a.toDateString() === b.toDateString();
// 0 = Sunday (default), 1 = Monday, 6 = Saturday. Per-device preference.
export const WEEK_START_KEY = "carteiro.weekStart";
export function weekStart() {
  try {
    const v = localStorage.getItem(WEEK_START_KEY);
    return v === null ? 0 : +v;
  } catch {
    return 0;
  }
}
function startOfWeek(d) {
  return addDays(startOfDay(d), -((d.getDay() - weekStart() + 7) % 7));
}
/** Weekday names in display order, e.g. ["Sun", "Mon", …]. */
function weekdayNames(style) {
  const base = new Date(2023, 0, 1 + weekStart()); // 2023-01-01 was a Sunday
  return Array.from({ length: 7 }, (_, i) => addDays(base, i).toLocaleDateString([], { weekday: style }));
}

/**
 * Event times as local Dates. Timed events are absolute instants; all-day and
 * "floating" ones are wall-clock values stored as UTC fields.
 */
// ---- time zones (the web view's Intl has the zone database) ------------------------

const LOCAL_TZ = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
const validTz = (tz) => {
  try { new Intl.DateTimeFormat("en", { timeZone: tz }); return true; } catch { return false; }
};
/// The wall clock of instant `ms` in zone `tz`, as numbers.
function zoneParts(tz, ms) {
  const p = Object.fromEntries(new Intl.DateTimeFormat("en-US", { timeZone: tz, hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" })
    .formatToParts(new Date(ms)).map((x) => [x.type, +x.value]));
  return p;
}
/// Seconds east of UTC in zone `tz` at instant `ms`.
function tzOffset(tz, ms) {
  const p = zoneParts(tz, ms);
  return Math.round((Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute, p.second) - Math.floor(ms / 1000) * 1000) / 1000);
}
/// Wall-clock seconds (written as if UTC) in zone `tz` -> the exact moment.
function fromZoned(wall, tz) {
  let t = wall * 1000 - tzOffset(tz, wall * 1000) * 1000;
  const second = tzOffset(tz, t); // the offset may differ around a DST change
  t = wall * 1000 - second * 1000;
  return new Date(t);
}
/// A moment as a local Date showing the wall clock in `tz` (for inputs).
function inZone(d, tz) {
  const p = zoneParts(tz, +d);
  return new Date(p.year, p.month - 1, p.day, p.hour, p.minute);
}
const zoneName = (tz) => tz.split("/").pop().replace(/_/g, " ");
const sameOffset = (tz, d) => tzOffset(tz, +d) === tzOffset(LOCAL_TZ, +d);

/// "10:00–11:00 New York time" for events whose zone runs on another offset.
function otherZone(e) {
  if (e.all_day || !e.tzid || !validTz(e.tzid) || sameOffset(e.tzid, evDate(e, "start"))) return "";
  const f = (d) => d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", timeZone: e.tzid });
  return `${f(evDate(e, "start"))}–${f(evDate(e, "end"))} ${zoneName(e.tzid)} time`;
}

function evDate(e, which) {
  const ts = e[which] * 1000;
  if (!e.all_day && !e.floating) return new Date(ts);
  // Wall-clock time in the event's own zone: convert to the exact moment.
  if (!e.all_day && e.tzid && validTz(e.tzid)) return fromZoned(e[which], e.tzid);
  const u = new Date(ts);
  return new Date(u.getUTCFullYear(), u.getUTCMonth(), u.getUTCDate(), u.getUTCHours(), u.getUTCMinutes());
}
const wallToUtc = (d) => Date.UTC(d.getFullYear(), d.getMonth(), d.getDate(), d.getHours(), d.getMinutes()) / 1000;

function range() {
  const a = C.anchor;
  switch (C.view) {
    case "day": return [a, addDays(a, 1)];
    case "week": { const s = startOfWeek(a); return [s, addDays(s, 7)]; }
    case "month": {
      const first = new Date(a.getFullYear(), a.getMonth(), 1);
      if (narrow()) {
        // Phones scroll through weeks: 3 months before the month at the top
        // and 5 after, so a screenful of weeks below it never reaches the end.
        const s = startOfWeek(new Date(a.getFullYear(), a.getMonth() - 3, 1));
        return [s, addDays(startOfWeek(new Date(a.getFullYear(), a.getMonth() + 5, 1)), 7)];
      }
      const s = startOfWeek(first);
      return [s, addDays(s, 42)];
    }
    default: { const s = startOfDay(a); return [s, addDays(s, 30)]; } // agenda: 30 days from the anchor
  }
}

// Your own answer to an event, shown on it: declined ones fade out.
const rsvpClass = (e) => (e.cancelled ? "cancelled" : e.rsvp ? `rsvp-${e.rsvp}` : "");
export const RSVP = [["yes", "Going"], ["maybe", "Maybe"], ["no", "Not going"]];
/// Your answer; for invitations also a button that sends it to the organizer
/// (picking an answer alone tells nobody).
export function rsvpButtons(e, invitation = isInvitation(e)) {
  if (e.cancelled) return html`<span class="cancelled-tag">Cancelled</span>`;
  return html`<div class="rsvp-row"><div class="rsvp" role="group" aria-label="Your answer">
    ${RSVP.map(([v, l]) => html`<button class="btn ${e.rsvp === v ? "on" : ""}" data-click="rsvp" data-id="${e.id}" data-v="${v}" aria-pressed="${e.rsvp === v}">${l}</button>`)}
  </div>${invitation ? html`<button class="btn primary send-rsvp" data-click="send-rsvp" data-id="${e.id}" ${e.rsvp ? "" : "disabled"} title="Send your answer for this date to the organizer">Send answer</button>` : ""}</div>`;
}
const isInvitation = (e) => !!e.remote_id && C.calendars.find((c) => c.id === e.calendar_id)?.remote_id === "invitations";

/// "Mon, 16 Nov, 10:00–10:30" (or the dates of an all-day event), in local time.
export function eventWhen(e) {
  const s = evDate(e, "start"), en = evDate(e, "end");
  const day = (d) => d.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" });
  if (e.all_day) {
    const last = addDays(en, -1);
    return +last > +s ? `${day(s)} – ${day(last)}` : day(s);
  }
  return `${day(s)}, ${fmtTime(s)}–${fmtTime(en)}${otherZone(e) ? ` (${otherZone(e)})` : ""}`;
}

const fmtTime = (d) => d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
const color = (e) => C.calendars.find((c) => c.id === e.calendar_id)?.color ?? "#0f6cbd";

// ---- loading ---------------------------------------------------------------------

export async function showCalendar() {
  C.view ??= narrow() ? "agenda" : "week";
  await load();
  if (!C.syncing) sync();
}

async function load() {
  const seq = ++C.loads;
  const [from, to] = range();
  // The small month in the side panel marks its days with events too.
  const [mFrom, mTo] = miniRange();
  const lo = Math.min(+from, +mFrom), hi = Math.max(+to, +mTo);
  // Pad by a day: all-day events are stored at UTC midnight.
  const [cals, evs] = await Promise.all([
    api.calendars(),
    api.events(Math.floor(lo / 1000) - 86400, Math.floor(hi / 1000) + 86400),
  ]);
  if (seq !== C.loads) return;
  C.calendars = cals ?? [];
  C.events = (evs ?? []).filter((e) => evDate(e, "end") > from && evDate(e, "start") < to);
  C.busy = new Set();
  for (const e of evs ?? []) {
    if (e.rsvp === "no") continue;
    // An all-day end is exclusive; a timed event ending at midnight doesn't reach that day.
    const last = new Date(+evDate(e, "end") - 1);
    for (let d = startOfDay(evDate(e, "start")); d <= last && d < mTo; d = addDays(d, 1)) {
      if (d >= mFrom) C.busy.add(d.toDateString());
    }
  }
  render();
}

async function sync() {
  C.syncing = true;
  render();
  try {
    const errors = await api.syncCalendars();
    if (errors?.length) toast(errors[0]);
  } catch (e) {
    toast(String(e));
  }
  C.syncing = false;
  await load();
}

// ---- rendering -------------------------------------------------------------------

function title() {
  const a = C.anchor;
  if (C.view === "day") return a.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: "numeric" });
  if (C.view === "week") {
    const s = startOfWeek(a), e = addDays(s, 6);
    return `${s.toLocaleDateString([], { day: "numeric", month: "short" })} – ${e.toLocaleDateString([], { day: "numeric", month: "short", year: "numeric" })}`;
  }
  if (C.view === "month") return a.toLocaleDateString([], { month: "long", year: "numeric" });
  if (sameDay(a, new Date())) return "Upcoming";
  const [s, e] = range();
  return `${s.toLocaleDateString([], { day: "numeric", month: "short" })} – ${addDays(e, -1).toLocaleDateString([], { day: "numeric", month: "short", year: "numeric" })}`;
}

function render() {
  if (narrow() && C.view === "week") C.view = "day"; // 7 columns don't fit a phone
  const views = narrow() ? [["agenda", "Agenda"], ["day", "Day"], ["month", "Month"]] : [["day", "Day"], ["week", "Week"], ["month", "Month"], ["agenda", "Agenda"]];
  put("#calendar", html`
    <div class="scrim" data-click="scrim"></div>
    <div class="cal-side">
      <button class="btn primary new" data-click="cal-new">${icon("plus", 16)} New event</button>
      ${miniMonth()}
      ${calendarList()}
    </div>
    <div class="cal-main">
      <header class="cal-bar">
        <button class="icon-btn show-narrow-flex" aria-label="Menu" data-click="menu">${icon("menu", 22)}</button>
        <button class="btn" data-click="cal-today">Today</button>
        <button class="icon-btn" aria-label="Previous" data-click="cal-prev">${icon("back")}</button>
        <button class="icon-btn" aria-label="Next" data-click="cal-next">${icon("chevron")}</button>
        <h2 class="cal-title ellipsis">${title()}</h2>
        <div class="seg">${views.map(([v, l]) => html`<button class="${C.view === v ? "on" : ""}" data-click="cal-view" data-view="${v}">${l}</button>`)}</div>
        <button class="icon-btn ${C.syncing ? "spin" : ""}" title="Sync calendars" data-click="cal-sync">${icon("sync")}</button>
        <input class="cal-search" type="search" placeholder="Search events" aria-label="Search events" data-input="cal-search" value="${C.query}" />
      </header>
      <div class="cal-body ${C.slide ? `slide-${C.slide}` : ""}">${C.query ? searchResults() : C.view === "month" ? (narrow() ? monthScroll() : month()) : C.view === "agenda" ? agenda() : timeGrid()}</div>
    </div>
    <button class="fab show-narrow-flex" aria-label="New event" data-click="cal-new">${icon("plus", 22)}</button>`);
  if (C.view === "day" || C.view === "week") {
    const grid = $("#calendar .grid-scroll");
    if (grid && !grid.dataset.scrolled) {
      grid.scrollTop = HOUR_PX * 7.5;
      grid.dataset.scrolled = "1";
    }
  }
  C.slide = null;
  if (C.view === "month" && narrow()) initMonthScroll();
}

const KIND_LABEL = { local: "On this device", holidays: "Holidays", ics: "Subscribed", caldav: "CalDAV", google: "Google", microsoft: "Microsoft 365 / Outlook" };

function removable(c) {
  if (c.source_kind === "local") return c.remote_id !== "local";
  // Mail-account calendars are removed with their account.
  return true;
}

function calendarList() {
  return html`<div class="cal-list">
    ${C.calendars.map((c) => html`
      <div class="cal-item" title="${KIND_LABEL[c.source_kind] ?? c.source_kind} · ${c.source_name}">
        <label><input type="checkbox" ${c.visible ? "checked" : ""} data-change="cal-visible" data-id="${c.id}" style="accent-color:${c.color ?? "#0f6cbd"}" />
          <span class="ellipsis">${c.name}</span></label>
        ${removable(c) ? html`<button class="icon-btn rm" title="Remove calendar" data-click="cal-remove" data-id="${c.id}">${icon("close", 13)}</button>` : ""}
      </div>`)}
    <button class="linkish add-cal" data-click="cal-add">${icon("plus", 14)} Add calendar</button>
  </div>`;
}

function miniRange() {
  const a = C.anchor;
  const s = startOfWeek(new Date(a.getFullYear(), a.getMonth(), 1));
  return [s, addDays(s, 42)];
}

function miniMonth() {
  const a = C.anchor;
  const [s] = miniRange();
  const today = new Date();
  return html`<div class="mini">
    <div class="mini-head">${a.toLocaleDateString([], { month: "long", year: "numeric" })}</div>
    <div class="mini-grid">
      ${weekdayNames("narrow").map((d) => html`<span class="muted">${d}</span>`)}
      ${Array.from({ length: 42 }, (_, i) => {
        const d = addDays(s, i);
        const busy = C.busy?.has(d.toDateString());
        const cls = [d.getMonth() !== a.getMonth() ? "other" : "", sameDay(d, today) ? "today" : "", sameDay(d, a) ? "sel" : "", busy ? "busy" : ""].join(" ");
        return html`<button class="${cls}" data-click="cal-goto" data-ts="${+d}" ${busy ? raw('aria-description="has events"') : ""}>${d.getDate()}</button>`;
      })}
    </div></div>`;
}

/** Side-by-side columns for overlapping timed events in one day. */
function layoutDay(evs) {
  const sorted = [...evs].sort((a, b) => a.s - b.s || b.e - a.e);
  const out = [];
  let cluster = [], clusterEnd = -Infinity;
  const flush = () => {
    const cols = [];
    for (const ev of cluster) {
      let c = cols.findIndex((end) => end <= ev.s);
      if (c < 0) { c = cols.length; cols.push(0); }
      cols[c] = ev.e;
      ev.col = c;
    }
    for (const ev of cluster) { ev.cols = cols.length; out.push(ev); }
    cluster = [];
  };
  for (const ev of sorted) {
    if (ev.s >= clusterEnd) { flush(); clusterEnd = -Infinity; }
    cluster.push(ev);
    clusterEnd = Math.max(clusterEnd, ev.e);
  }
  flush();
  return out;
}

function timeGrid() {
  const [from] = range();
  const days = C.view === "day" ? [from] : Array.from({ length: 7 }, (_, i) => addDays(from, i));
  const today = new Date();
  const allDay = (d) => C.events.filter((e) => e.all_day && evDate(e, "start") < addDays(d, 1) && evDate(e, "end") > d);
  const timed = (d) => {
    const dayStart = +d, dayEnd = +addDays(d, 1);
    return layoutDay(
      C.events
        .filter((ev) => !ev.all_day && +evDate(ev, "start") < dayEnd && +evDate(ev, "end") > dayStart)
        .map((ev) => ({
          ev,
          s: Math.max(+evDate(ev, "start"), dayStart),
          // Very short events still get a visible block.
          e: Math.min(Math.max(+evDate(ev, "end"), +evDate(ev, "start") + 15 * 60000), dayEnd),
        })),
    );
  };
  const nowTop = (today.getHours() * 60 + today.getMinutes()) * HOUR_PX / 60;
  return html`
    <div class="tg-head" style="grid-template-columns: 56px repeat(${days.length}, 1fr)">
      <span></span>
      ${days.map((d) => html`<div class="tg-day ${sameDay(d, today) ? "today" : ""}">
        <span class="dow">${d.toLocaleDateString([], { weekday: "short" })}</span><span class="dom">${d.getDate()}</span></div>`)}
      <span class="muted small tg-allday-label">all day</span>
      ${days.map((d) => html`<div class="tg-allday">${allDay(d).map((e) => html`<button class="chip ${rsvpClass(e)}" style="--c:${color(e)}" data-click="cal-open" data-id="${e.id}">${e.title || "(no title)"}</button>`)}</div>`)}
    </div>
    <div class="grid-scroll">
      <div class="tg" style="grid-template-columns: 56px repeat(${days.length}, 1fr); height:${24 * HOUR_PX}px">
        <div class="tg-hours">${Array.from({ length: 24 }, (_, h) => html`<span style="top:${h * HOUR_PX}px">${h ? fmtTime(new Date(2000, 0, 1, h)) : ""}</span>`)}</div>
        ${days.map((d) => html`<div class="tg-col" data-click="cal-slot" data-ts="${+d}">
          ${sameDay(d, today) ? html`<div class="now-line" style="top:${nowTop}px"></div>` : ""}
          ${timed(d).map(({ ev, s, e, col, cols }) => {
            const top = (s - +d) / 3600000 * HOUR_PX, height = Math.max((e - s) / 3600000 * HOUR_PX, 18);
            return html`<button class="ev ${rsvpClass(ev)}" data-click="cal-open" data-id="${ev.id}"
              style="--c:${color(ev)};top:${top}px;height:${height}px;left:calc(${(col / cols) * 100}% + 1px);width:calc(${100 / cols}% - 3px)">
              <strong class="ellipsis">${ev.title || "(no title)"}</strong>
              ${height > 34 ? html`<span class="ellipsis">${fmtTime(evDate(ev, "start"))}${ev.location ? ` · ${ev.location}` : ""}</span>` : ""}
            </button>`;
          })}
        </div>`)}
      </div>
    </div>`;
}

function month() {
  const [from] = range();
  const today = new Date();
  return html`<div class="month">
    ${weekdayNames("short").map((d) => html`<div class="mh muted small">${d}</div>`)}
    ${Array.from({ length: 42 }, (_, i) => {
      const d = addDays(from, i);
      const evs = C.events.filter((e) => evDate(e, "start") < addDays(d, 1) && evDate(e, "end") > d).sort((a, b) => b.all_day - a.all_day || a.start - b.start);
      return html`<div class="mc ${d.getMonth() !== C.anchor.getMonth() ? "other" : ""}" data-click="cal-slot" data-ts="${+d + 9 * 3600000}">
        <span class="dn ${sameDay(d, today) ? "today" : ""}">${d.getDate()}</span>
        ${evs.slice(0, 3).map((e) => html`<button class="chip ${e.all_day ? "" : "timed"} ${rsvpClass(e)}" style="--c:${color(e)}" data-click="cal-open" data-id="${e.id}">
          ${e.all_day ? "" : html`<span class="t">${fmtTime(evDate(e, "start"))}</span>`}${e.title || "(no title)"}</button>`)}
        ${evs.length > 3 ? html`<button class="more" data-click="cal-goto-day" data-ts="${+d}">+${evs.length - 3} more</button>` : ""}
      </div>`;
    })}
  </div>`;
}

// Phones: the month view is a continuous run of weeks scrolled vertically; the
// title follows the month at the top, and more weeks load near either end.
const monthKey = (d) => d.getFullYear() * 12 + d.getMonth();

function monthCell(d, today) {
  const evs = C.events.filter((e) => evDate(e, "start") < addDays(d, 1) && evDate(e, "end") > d).sort((a, b) => b.all_day - a.all_day || a.start - b.start);
  return html`<div class="mc ${monthKey(d) !== monthKey(C.anchor) ? "other" : ""}" data-m="${monthKey(d)}" data-click="cal-slot" data-ts="${+d + 9 * 3600000}">
    <span class="dn ${sameDay(d, today) ? "today" : ""}">${d.getDate()}</span>
    ${evs.slice(0, 3).map((e) => html`<button class="chip ${e.all_day ? "" : "timed"} ${rsvpClass(e)}" style="--c:${color(e)}" data-click="cal-open" data-id="${e.id}">
      ${e.all_day ? "" : html`<span class="t">${fmtTime(evDate(e, "start"))}</span>`}${e.title || "(no title)"}</button>`)}
    ${evs.length > 3 ? html`<button class="more" data-click="cal-goto-day" data-ts="${+d}">+${evs.length - 3} more</button>` : ""}
  </div>`;
}

function monthScroll() {
  const [from, to] = range();
  const today = new Date();
  const weeks = [];
  for (let w = from; w < to; w = addDays(w, 7)) weeks.push(w);
  return html`<div class="month-scroll">
    <div class="ms-head">${weekdayNames("short").map((d) => html`<div class="mh muted small">${d}</div>`)}</div>
    ${weeks.map((w) => html`<div class="ms-week" data-week="${+w}">${Array.from({ length: 7 }, (_, i) => monthCell(addDays(w, i), today))}</div>`)}
  </div>`;
}

function initMonthScroll() {
  const body = $("#calendar .cal-body");
  const head = body?.querySelector(".ms-head");
  if (!head) return;
  const target = C.keepWeek ?? +startOfWeek(new Date(C.anchor.getFullYear(), C.anchor.getMonth(), 1));
  C.keepWeek = null;
  const row = body.querySelector(`[data-week="${target}"]`);
  if (row) body.scrollTop = row.offsetTop - head.offsetHeight;
  let queued = false;
  body.onscroll = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; monthScrolled(body, head); });
  };
}

function monthScrolled(body, head) {
  const rows = [...body.querySelectorAll(".ms-week")];
  const top = rows.find((r) => r.offsetTop + r.offsetHeight / 2 > body.scrollTop + head.offsetHeight) ?? rows[rows.length - 1];
  // A week belongs to the month of its last day (the week holding the 1st starts the month).
  const last = addDays(new Date(+top.dataset.week), 6);
  if (monthKey(last) !== monthKey(C.anchor)) {
    C.anchor = new Date(last.getFullYear(), last.getMonth(), 1);
    for (const c of body.querySelectorAll(".mc")) c.classList.toggle("other", +c.dataset.m !== monthKey(C.anchor));
    $("#calendar .cal-title").textContent = title();
  }
  // Within a few weeks of either end of what's loaded: reload around this
  // month, keeping the view still.
  const margin = 3 * top.offsetHeight;
  const nearEnd = body.scrollTop < margin + head.offsetHeight || body.scrollTop + body.clientHeight > body.scrollHeight - margin;
  // Only when that loads different weeks (never reload in a loop).
  if (nearEnd && +range()[0] !== +rows[0].dataset.week) {
    C.keepWeek = +top.dataset.week;
    body.onscroll = null;
    load();
  }
}

function agenda() {
  const [from, to] = range();
  const days = [];
  for (let d = from; d < to; d = addDays(d, 1)) {
    const evs = C.events.filter((e) => evDate(e, "start") < addDays(d, 1) && evDate(e, "end") > d).sort((a, b) => b.all_day - a.all_day || a.start - b.start);
    if (evs.length || sameDay(d, new Date())) days.push([d, evs]);
  }
  if (!days.length) return html`<div class="agenda"><p class="muted empty">Nothing scheduled in this period.</p></div>`;
  return html`<div class="agenda">${days.map(([d, evs]) => html`
    <section>
      <h3 class="${sameDay(d, new Date()) ? "today" : ""}">${d.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" })}</h3>
      ${evs.length ? evs.map((e) => html`<button class="ag-ev ${rsvpClass(e)}" data-click="cal-open" data-id="${e.id}">
        <span class="bar" style="background:${color(e)}"></span>
        <span class="when muted small">${e.all_day ? "All day" : html`${fmtTime(evDate(e, "start"))}<br />${fmtTime(evDate(e, "end"))}`}</span>
        <span class="what"><strong>${e.title || "(no title)"}</strong>${e.location ? html`<span class="muted small">${e.location}</span>` : ""}${otherZone(e) ? html`<span class="muted small">${otherZone(e)}</span>` : ""}</span>
      </button>`) : html`<p class="muted small">Nothing scheduled</p>`}
    </section>`)}</div>`;
}

// ---- editor ----------------------------------------------------------------------

const pad = (n) => String(n).padStart(2, "0");
const dateVal = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const timeVal = (d) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
function parseLocal(date, time) {
  const [y, m, d] = date.split("-").map(Number);
  const [h, mi] = (time || "00:00").split(":").map(Number);
  return new Date(y, m - 1, d, h, mi);
}

// The mail a meeting came from: the invitation, updates, a cancellation.
const SOURCE_KIND = { added: "Invitation", cancelled: "Cancellation", answer: "Reply", "older update": "Older update" };
async function showSources(ev) {
  const list = await api.eventSources(ev.id).catch(() => []);
  const box = $("#modal .ev-sources");
  if (!box || !list.length) return;
  box.hidden = false;
  put(box, html`<span class="label">From mail</span>${list.map((m) => html`
    <button class="linkish ev-source" data-click="ev-source" data-acct="${m.account_id}" data-key="${m.thread_key}" data-id="${m.message_id}">
      ${icon("read", 14)} <strong>${SOURCE_KIND[m.outcome] ?? "Message"}</strong> from ${m.from} · ${new Date(m.date * 1000).toLocaleDateString([], { day: "numeric", month: "short", year: "numeric" })}
      <span class="muted small ellipsis">${m.subject}</span></button>`)}`);
}
on("click", "ev-source", (el) => {
  closeEditor();
  showMessage(+el.dataset.acct, el.dataset.key, +el.dataset.id);
});

function openEditor(ev) {
  const writable = C.calendars.filter((c) => !c.read_only);
  if (!writable.length) return toast("No writable calendar");
  C.editing = ev;
  const tz = ev.tzid && validTz(ev.tzid) ? ev.tzid : LOCAL_TZ;
  const shown = (d) => (ev.all_day || tz === LOCAL_TZ ? d : inZone(d, tz));
  const s = shown(evDate(ev, "start"));
  // All-day end is exclusive; show the last day inclusively.
  const e = ev.all_day ? addDays(evDate(ev, "end"), -1) : shown(evDate(ev, "end"));
  const zones = [...new Set([LOCAL_TZ, tz, "UTC", ...(Intl.supportedValuesOf?.("timeZone") ?? [])])];
  const cal = C.calendars.find((c) => c.id === ev.calendar_id);
  const locked = !!ev.id && ev.recurring && cal?.source_kind !== "local";
  put("#modal", html`
    <div class="modal-backdrop" data-click="cal-backdrop">
      <div class="modal" role="dialog" aria-label="Event">
        <header>${ev.id ? "Edit event" : "New event"}<button class="icon-btn" aria-label="Close" data-click="cal-close">${icon("close")}</button></header>
        <div class="content form">
          ${locked ? html`<div class="note">This is part of a repeating series; change the series in the web calendar.</div>` : ""}
          <input name="title" class="ev-title" placeholder="Add a title" value="${ev.title ?? ""}" />
          <label class="stack"><span>Calendar</span>
            <select name="calendar">${writable.map((c) => html`<option value="${c.id}" ${c.id === ev.calendar_id ? "selected" : ""}>${c.name} — ${c.source_name}</option>`)}</select></label>
          <label class="check"><input type="checkbox" name="all_day" ${ev.all_day ? "checked" : ""} data-change="ev-allday" /> All day</label>
          <div class="ev-times">
            <label class="stack"><span>Starts</span><span class="row2"><input type="date" name="sd" value="${dateVal(s)}" data-change="ev-when" /><input type="time" name="st" value="${timeVal(s)}" ${ev.all_day ? "hidden" : ""} data-change="ev-when" /></span></label>
            <label class="stack"><span>Ends</span><span class="row2"><input type="date" name="ed" value="${dateVal(e)}" data-change="ev-when" /><input type="time" name="et" value="${timeVal(e)}" ${ev.all_day ? "hidden" : ""} data-change="ev-when" /></span></label>
          </div>
          <label class="stack ev-tz" ${ev.all_day ? "hidden" : ""}><span>Time zone</span>
            <select name="tz" data-change="ev-when">${zones.map((z) => html`<option value="${z}" ${z === tz ? "selected" : ""}>${z === LOCAL_TZ ? `${z} (yours)` : z}</option>`)}</select></label>
          <p class="muted small tz-note" hidden></p>
          ${ev.id ? html`<div class="stack"><span class="label">Will you go? <span class="muted small">${isInvitation(ev) ? "(kept here unless you send it)" : "(only noted here)"}</span></span>${rsvpButtons(ev)}</div>` : ""}
          <label class="stack"><span>Location</span><input name="location" value="${ev.location ?? ""}" /></label>
          <label class="stack"><span>Notes</span><textarea name="description" rows="4">${ev.description ?? ""}</textarea></label>
          ${ev.id && ev.remote_id ? html`<div class="ev-sources" hidden></div>` : ""}
          <div class="error" hidden></div>
        </div>
        <footer>
          ${ev.id && !locked ? html`<button class="btn danger" data-click="ev-delete">Delete</button>` : ""}
          <span class="spacer"></span>
          <button class="btn" data-click="cal-close">Cancel</button>
          ${locked ? "" : html`<button class="btn primary" data-click="ev-save">Save</button>`}
        </footer>
      </div>
    </div>`);
  $("#modal [name=title]").focus();
  tzNote();
  if (ev.id && ev.remote_id) showSources(ev);
}

/// The times in your own zone, when the event's zone runs on another offset.
function tzNote() {
  const note = $("#modal .tz-note");
  if (!note) return;
  const { event } = readEditor();
  const zoned = event.floating && event.tzid;
  const s = evDate(event, "start");
  note.hidden = !zoned || sameOffset(event.tzid, s);
  if (!note.hidden) note.textContent = `Your time (${zoneName(LOCAL_TZ)}): ${s.toLocaleString([], { weekday: "short", hour: "numeric", minute: "2-digit" })} – ${evDate(event, "end").toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
}
on("change", "ev-when", () => tzNote());

function readEditor() {
  const v = (n) => $(`#modal [name=${n}]`);
  const allDay = v("all_day").checked;
  let start, end;
  if (allDay) {
    const s = parseLocal(v("sd").value), e = addDays(parseLocal(v("ed").value), 1);
    start = wallToUtc(s);
    end = Math.max(wallToUtc(e), start + 86400);
  } else {
    start = Math.floor(+parseLocal(v("sd").value, v("st").value) / 1000);
    end = Math.floor(+parseLocal(v("ed").value, v("et").value) / 1000);
    if (end < start) end = start + 1800;
  }
  const tz = v("tz")?.value ?? LOCAL_TZ;
  const base = { title: v("title").value.trim(), location: v("location").value.trim(), description: v("description").value, all_day: allDay, recurring: false, uid: "" };
  if (allDay || tz === LOCAL_TZ || !validTz(tz)) {
    return { calendar: +v("calendar").value, event: { ...base, start, end, floating: false, tzid: "" } };
  }
  // Another zone: the typed times are wall-clock times there.
  const wall = (d, t) => { const [y, m, dd] = d.split("-").map(Number); const [h, mi] = t.split(":").map(Number); return Date.UTC(y, m - 1, dd, h, mi) / 1000; };
  const ws = wall(v("sd").value, v("st").value);
  const we = Math.max(wall(v("ed").value, v("et").value), ws);
  return {
    calendar: +v("calendar").value,
    event: { ...base, start: ws, end: we === ws ? ws + 1800 : we, floating: true, tzid: tz, tz_offset: tzOffset(tz, +fromZoned(ws, tz)) },
  };
}

const closeEditor = () => { C.editing = null; put("#modal", ""); };

on("click", "cal-close", closeEditor);
on("click", "cal-backdrop", (el, e) => e.target === el && closeEditor());
on("change", "ev-allday", (el) => { for (const t of document.querySelectorAll("#modal [type=time], #modal .ev-tz")) t.hidden = el.checked; tzNote(); });
on("click", "ev-save", async (el) => {
  const { calendar, event } = readEditor();
  el.disabled = true;
  try {
    await api.saveEvent(calendar, C.editing.id ?? null, event);
    closeEditor();
    await load();
  } catch (e) {
    const err = $("#modal .error");
    err.textContent = String(e);
    err.hidden = false;
    el.disabled = false;
  }
});
// A series laid out on this device (an invitation): remove one date, a
// period, everything from a date on, or all of it.
async function removeFromSeries(ev) {
  const day = evDate(ev, "start");
  const later = addDays(day, 28);
  const r = await choose("Remove which dates of this repeating event?", [
    ["one", `Only ${day.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" })}`],
    ["after", "This one and all later ones"],
    ["all", "The whole series", true],
  ], html`<p class="muted small period-title">Or every one in a period:</p>
    <div class="period">
      <label>From<input type="date" name="p-from" value="${dateVal(day)}" /></label>
      <label>Until (inclusive)<input type="date" name="p-to" value="${dateVal(later)}" /></label>
    </div>
    <div class="choices"><button class="btn" data-ask="period">Remove this period</button></div>`, (box) => [box.querySelector("[name=p-from]").value, box.querySelector("[name=p-to]").value]);
  if (!r) return false;
  // Bounds in the event's own representation: wall clock for zoned/floating
  // and all-day events, exact time otherwise.
  const wall = ev.all_day || ev.floating;
  const at = (d) => (wall ? wallToUtc(d) : Math.floor(+d / 1000));
  const startOf = (s) => { const [y, m, d] = s.split("-").map(Number); return new Date(y, m - 1, d); };
  let from = null, to = null;
  if (r.choice === "one") { from = ev.start; to = ev.start + 1; }
  else if (r.choice === "after") from = ev.start;
  else if (r.choice === "period") {
    const [a, b] = r.value;
    if (!a || !b || b < a) { toast("Pick a start date and an end date after it"); return false; }
    from = at(startOf(a));
    to = at(addDays(startOf(b), 1));
  }
  const n = await api.removeSeries(ev.id, from, to);
  toast(n === 1 ? "Removed 1 date" : `Removed ${n} dates`);
  return true;
}

on("click", "ev-delete", async () => {
  const ev = C.editing;
  if (ev.recurring && ev.remote_id && C.calendars.find((c) => c.id === ev.calendar_id)?.source_kind === "local") {
    try {
      if (await removeFromSeries(ev)) { closeEditor(); await load(); }
    } catch (e) { toast(String(e)); }
    return;
  }
  if (!(await ask("Delete this event?"))) return;
  try {
    await api.deleteEvent(C.editing.id);
    closeEditor();
    await load();
  } catch (e) { toast(String(e)); }
});

// ---- navigation --------------------------------------------------------------------

function step(dir) {
  C.keepWeek = null;
  const a = C.anchor;
  C.anchor = C.view === "month" ? new Date(a.getFullYear(), a.getMonth() + dir, 1) : addDays(a, dir * (C.view === "week" ? 7 : C.view === "agenda" ? 30 : 1));
  load();
}

// Swipe left / right in any view for the next / previous period. Mostly
// vertical gestures are scrolls, not swipes.
let touch = null;
document.addEventListener("touchstart", (e) => {
  touch = e.touches.length === 1 && e.target.closest?.("#calendar .cal-body") && !$("#modal .modal")
    ? { x: e.touches[0].clientX, y: e.touches[0].clientY, t: Date.now() } : null;
}, { passive: true });
document.addEventListener("touchend", (e) => {
  if (!touch) return;
  const p = e.changedTouches[0];
  const dx = p.clientX - touch.x, dy = p.clientY - touch.y, quick = Date.now() - touch.t < 700;
  touch = null;
  if (quick && Math.abs(dx) > 60 && Math.abs(dx) > 2 * Math.abs(dy)) {
    C.slide = dx < 0 ? "next" : "prev";
    step(dx < 0 ? 1 : -1);
  }
}, { passive: true });

on("click", "cal-prev", () => step(-1));
on("click", "cal-next", () => step(1));
on("click", "cal-today", () => { C.keepWeek = null; C.anchor = startOfDay(new Date()); load(); });
on("click", "cal-view", (el) => { C.keepWeek = null; C.view = el.dataset.view; load(); });
on("click", "cal-sync", () => sync());
on("click", "cal-goto", (el) => { C.anchor = new Date(+el.dataset.ts); closeDrawer(); load(); });
on("click", "cal-goto-day", (el, e) => { e.stopPropagation(); C.anchor = new Date(+el.dataset.ts); C.view = "day"; load(); });
on("change", "cal-visible", async (el) => { await api.setCalendarVisible(+el.dataset.id, el.checked); load(); });
// ---- search ------------------------------------------------------------------------

function searchResults() {
  if (!C.results.length) return html`<div class="agenda"><p class="muted empty">No events match "${C.query}".</p></div>`;
  const days = [];
  for (const e of C.results) {
    const d = startOfDay(evDate(e, "start"));
    const last = days[days.length - 1];
    if (last && sameDay(last[0], d)) last[1].push(e);
    else days.push([d, [e]]);
  }
  return html`<div class="agenda"><p class="muted small">${C.results.length} event${C.results.length === 1 ? "" : "s"} matching "${C.query}"</p>
    ${days.map(([d, evs]) => html`<section>
      <h3 class="${sameDay(d, new Date()) ? "today" : ""}">${d.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: "numeric" })}</h3>
      ${evs.map((e) => html`<button class="ag-ev ${rsvpClass(e)}" data-click="cal-open" data-id="${e.id}">
        <span class="bar" style="background:${color(e)}"></span>
        <span class="when muted small">${e.all_day ? "All day" : html`${fmtTime(evDate(e, "start"))}<br />${fmtTime(evDate(e, "end"))}`}</span>
        <span class="what"><strong>${e.title || "(no title)"}</strong>${e.location ? html`<span class="muted small">${e.location}</span>` : ""}${otherZone(e) ? html`<span class="muted small">${otherZone(e)}</span>` : ""}</span>
      </button>`)}
    </section>`)}</div>`;
}

let searchTimer;
on("input", "cal-search", (el) => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(async () => {
    C.query = el.value.trim();
    C.results = C.query ? ((await api.searchEvents(C.query).catch((e) => (toast(String(e)), []))) ?? []) : [];
    // Only the results redraw, so the search box keeps focus while typing.
    put("#calendar .cal-body", C.query ? searchResults() : C.view === "month" ? (narrow() ? monthScroll() : month()) : C.view === "agenda" ? agenda() : timeGrid());
    if (!C.query && C.view === "month" && narrow()) initMonthScroll();
  }, 200);
});

on("click", "cal-open", (el, e) => {
  e.stopPropagation();
  const ev = C.events.find((x) => x.id === +el.dataset.id) ?? C.results.find((x) => x.id === +el.dataset.id);
  if (ev) openEditor(ev);
});
function newEventAt(start) {
  const cal = C.calendars.find((c) => !c.read_only && c.visible) ?? C.calendars.find((c) => !c.read_only);
  const s = Math.floor(+start / 1000);
  openEditor({ id: null, calendar_id: cal?.id, title: "", location: "", description: "", start: s, end: s + 3600, all_day: false, floating: false, recurring: false });
}
on("click", "cal-new", () => {
  closeDrawer();
  const d = new Date();
  d.setMinutes(0, 0, 0);
  d.setHours(d.getHours() + 1);
  newEventAt(sameDay(C.anchor, new Date()) ? d : new Date(+C.anchor + 9 * 3600000));
});
on("click", "cal-slot", (el, e) => {
  if (e.target.closest("[data-click=cal-open], .more")) return;
  if (el.classList.contains("tg-col")) {
    // Snap the click position to the half hour.
    const y = e.clientY - el.getBoundingClientRect().top;
    const mins = Math.floor((y / HOUR_PX) * 2) * 30;
    newEventAt(new Date(+el.dataset.ts + mins * 60000));
  } else {
    newEventAt(new Date(+el.dataset.ts));
  }
});

// ---- managing calendars ---------------------------------------------------------------

function modal(title, body, footer = "") {
  put("#modal", html`
    <div class="modal-backdrop" data-click="cal-backdrop">
      <div class="modal" role="dialog" aria-label="${title}">
        <header>${title}<button class="icon-btn" aria-label="Close" data-click="cal-close">${icon("close")}</button></header>
        <div class="content form">${body}</div>
        ${footer ? html`<footer>${footer}</footer>` : ""}
      </div>
    </div>`);
}

function modalError(e) {
  const el = $("#modal .error");
  if (el) { el.textContent = String(e); el.hidden = false; }
}

// Phones: the sidebar is hidden, so list calendars in a sheet.
on("click", "cal-manage", () => modal("Calendars", calendarList()));

on("click", "cal-remove", async (el) => {
  const c = C.calendars.find((x) => x.id === +el.dataset.id);
  if (!c) return;
  const what = c.source_kind === "local" ? `the calendar "${c.name}" and its events` : `"${c.source_name}" from Carteiro (nothing is deleted on the server)`;
  if (!(await ask(`Remove ${what}?`, "Remove"))) return;
  try {
    if (c.source_kind === "local") await api.removeLocalCalendar(c.id);
    else await api.removeCalSource(c.source_id);
    put("#modal", "");
    await load();
  } catch (e) { toast(String(e)); }
});

const ES_REGIONS = {
  "ES-AN": "Andalucía", "ES-AR": "Aragón", "ES-AS": "Asturias", "ES-CB": "Cantabria", "ES-CE": "Ceuta", "ES-CL": "Castilla y León",
  "ES-CM": "Castilla-La Mancha", "ES-CN": "Canarias", "ES-CT": "Catalunya", "ES-EX": "Extremadura", "ES-GA": "Galicia", "ES-IB": "Illes Balears",
  "ES-MC": "Murcia", "ES-MD": "Madrid", "ES-ML": "Melilla", "ES-NC": "Navarra", "ES-PV": "País Vasco", "ES-RI": "La Rioja", "ES-VC": "Comunitat Valenciana",
};

on("click", "cal-add", () => {
  modal("Add calendar", html`
    <div class="add-cal-grid">
      <button class="add-opt" data-click="cal-add-holidays"><strong>Holidays</strong><span class="muted small">Public holidays of any country and region</span></button>
      <button class="add-opt" data-click="oauth" data-provider="microsoft" data-what="calendar"><strong>Microsoft 365 / Outlook</strong><span class="muted small">Work or personal Outlook calendar (sign in)</span></button>
      <button class="add-opt" data-click="oauth" data-provider="google" data-what="calendar"><strong>Google Calendar</strong><span class="muted small">Sign in with Google</span></button>
      <button class="add-opt" data-click="caldav-add"><strong>CalDAV</strong><span class="muted small">Nextcloud, Fastmail, iCloud, Radicale…</span></button>
      <button class="add-opt" data-click="cal-add-ics"><strong>Subscribe from a link</strong><span class="muted small">An .ics / webcal address (read-only)</span></button>
      <button class="add-opt" data-click="cal-add-local"><strong>New calendar on this device</strong><span class="muted small">Works offline, not synced</span></button>
    </div>`);
});

on("click", "cal-add-holidays", async () => {
  modal("Holiday calendar", html`
    <label class="stack"><span>Country</span><select name="country" data-change="hol-country"><option value="">Loading…</option></select></label>
    <label class="stack"><span>Region (adds regional holidays)</span><select name="region"><option value="">National holidays only</option></select></label>
    <p class="muted small">Holiday data from Nager.Date (open data, no account).</p>
    <div class="error" hidden></div>`,
    html`<button class="btn" data-click="cal-add">Back</button><button class="btn primary" data-click="hol-save">Add</button>`);
  try {
    const countries = await api.holidayCountries();
    const guess = (navigator.language.split("-")[1] ?? "").toUpperCase();
    put("#modal [name=country]", html`<option value="">Choose a country…</option>${countries.map((c) => html`<option value="${c.code}" ${c.code === guess ? "selected" : ""}>${c.name}</option>`)}`);
    if (guess) loadRegions(guess);
  } catch (e) { modalError(e); }
});

async function loadRegions(country) {
  const sel = $("#modal [name=region]");
  put(sel, html`<option value="">National holidays only</option>`);
  if (!country) return;
  try {
    const regions = await api.holidayRegions(country);
    put(sel, html`<option value="">National holidays only</option>${regions.map((r) => html`<option value="${r}">${ES_REGIONS[r] ? `${ES_REGIONS[r]} (${r})` : r}</option>`)}`);
  } catch (e) { modalError(e); }
}
on("change", "hol-country", (el) => loadRegions(el.value));

on("click", "hol-save", async (el) => {
  const c = $("#modal [name=country]"), r = $("#modal [name=region]");
  if (!c.value) return modalError("Choose a country");
  const name = `Holidays: ${c.selectedOptions[0].textContent}${r.value ? ` (${ES_REGIONS[r.value] ?? r.value})` : ""}`;
  el.disabled = true;
  el.textContent = "Adding…";
  try {
    await api.addHolidayCalendar(c.value, r.value || null, name);
    put("#modal", "");
    await load();
  } catch (e) { modalError(e); el.disabled = false; el.textContent = "Add"; }
});

on("click", "cal-add-ics", () => {
  modal("Subscribe to a calendar", html`
    <label class="stack"><span>Calendar link</span><input name="ics-url" placeholder="https://… .ics or webcal://…" autocapitalize="off" spellcheck="false" /></label>
    <label class="stack"><span>Name</span><input name="ics-name" /></label>
    <p class="muted small">Subscribed calendars are read-only and refresh on every sync.</p>
    <div class="error" hidden></div>`,
    html`<button class="btn" data-click="cal-add">Back</button><button class="btn primary" data-click="ics-save">Subscribe</button>`);
});
on("click", "ics-save", async (el) => {
  el.disabled = true;
  try {
    await api.addIcsCalendar($("#modal [name=ics-url]").value, $("#modal [name=ics-name]").value);
    put("#modal", "");
    await load();
  } catch (e) { modalError(e); el.disabled = false; }
});

const COLORS = ["#0f6cbd", "#c239b3", "#107c10", "#ca5010", "#8764b8", "#038387", "#c50f1f", "#986f0b"];
on("click", "cal-add-local", () => {
  modal("New calendar", html`
    <label class="stack"><span>Name</span><input name="local-name" placeholder="e.g. Family" /></label>
    <div class="swatches">${COLORS.map((c, i) => html`<label><input type="radio" name="local-color" value="${c}" ${i === 1 ? "checked" : ""} /><span style="background:${c}"></span></label>`)}</div>
    <div class="error" hidden></div>`,
    html`<button class="btn" data-click="cal-add">Back</button><button class="btn primary" data-click="local-save">Create</button>`);
  $("#modal [name=local-name]").focus();
});
on("click", "local-save", async () => {
  const name = $("#modal [name=local-name]").value.trim();
  if (!name) return modalError("Give the calendar a name");
  try {
    await api.addLocalCalendar(name, $("#modal [name=local-color]:checked").value);
    put("#modal", "");
    await load();
  } catch (e) { modalError(e); }
});

// Added from the shared sign-in / CalDAV dialogs.
window.addEventListener("calendars-changed", () => {
  if (!$("#calendar").hidden) sync();
});

/** Re-render after the preference changes. */
/// Opens the calendar on the day of event `e`.
export async function showEventDay(e) {
  document.querySelector("[data-click=mode][data-mode=calendar]")?.click();
  C.keepWeek = null;
  C.anchor = startOfDay(evDate(e, "start"));
  await load();
}

export function refreshCalendar() {
  if (!$("#calendar").hidden) load();
}

// Your answer is bookkeeping on this device only: the organizer and the other
// guests are never sent anything.
on("click", "rsvp", async (el) => {
  const id = +el.dataset.id;
  const v = el.classList.contains("on") ? "" : el.dataset.v;
  try {
    await api.setRsvp(id, v);
  } catch (e) { return toast(String(e)); }
  for (const b of document.querySelectorAll(`[data-click=rsvp][data-id="${id}"]`)) {
    b.classList.toggle("on", b.dataset.v === v);
    b.setAttribute("aria-pressed", b.dataset.v === v);
  }
  for (const b of document.querySelectorAll(`[data-click=send-rsvp][data-id="${id}"]`)) b.disabled = !v;
  if (C.editing?.id === id) C.editing.rsvp = v;
  document.dispatchEvent(new CustomEvent("rsvp-changed", { detail: { id, rsvp: v } }));
  if (C.events) load();
});

on("click", "send-rsvp", async (el) => {
  el.disabled = true;
  el.textContent = "Sending…";
  try {
    const r = await api.sendRsvp(+el.dataset.id);
    toast(r === "queued" ? "No connection: your answer is in the Outbox and goes out when you're back online" : "Your answer was sent to the organizer");
  } catch (e) { toast(String(e)); }
  el.disabled = false;
  el.textContent = "Send answer";
});
